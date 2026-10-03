//! HTML support for Razor files (`.razor`, `.cshtml`).
//!
//! The Roslyn language server answers for the C# and Razor parts of such a
//! file itself. For the HTML it generates a plain HTML document with the same
//! layout as the Razor file and hands it to the editor (`razor/updateHtml`).
//! Requests it wants answered from that document then come back to the editor
//! under their usual method names, in an envelope that names the Razor
//! document, the checksum of the HTML the server expects, and the original
//! parameters. The editor is expected to pass them on to an HTML language
//! server and to return its answer.
//!
//! This module does that: it keeps the generated documents open on the
//! language server configured for `html` and relays the requests.

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use helix_lsp::{jsonrpc, lsp, Client, LanguageServerId};
use helix_view::Editor;
use serde::Deserialize;
use serde_json::Value;

const UPDATE_HTML: &str = "razor/updateHtml";
/// Appended to the URI of a Razor document to name its generated HTML.
const HTML_SUFFIX: &str = "__virtual.html";
/// The Razor server holds back its own answer until the HTML one has arrived,
/// so a slow HTML server may not keep it waiting for long.
const FORWARD_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UpdateHtml {
    text_document: lsp::TextDocumentIdentifier,
    checksum: String,
    text: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ForwardedRequest {
    text_document: lsp::TextDocumentIdentifier,
    checksum: String,
    request: Value,
}

struct HtmlDocument {
    uri: lsp::Url,
    checksum: String,
    text: String,
    version: i32,
    /// The version each HTML language server has last been sent. Empty while
    /// the Razor document is closed.
    synced: HashMap<LanguageServerId, i32>,
    /// Whether an HTML language server has been looked for since the Razor
    /// document was opened.
    looked_for_server: bool,
}

impl HtmlDocument {
    fn sync(&mut self, server: &Client) {
        match self.synced.insert(server.id(), self.version) {
            Some(version) if version == self.version => {}
            Some(_) => server.notify::<lsp::notification::DidChangeTextDocument>(
                lsp::DidChangeTextDocumentParams {
                    text_document: lsp::VersionedTextDocumentIdentifier::new(
                        self.uri.clone(),
                        self.version,
                    ),
                    content_changes: vec![lsp::TextDocumentContentChangeEvent {
                        range: None,
                        range_length: None,
                        text: self.text.clone(),
                    }],
                },
            ),
            None => server.notify::<lsp::notification::DidOpenTextDocument>(
                lsp::DidOpenTextDocumentParams {
                    text_document: lsp::TextDocumentItem::new(
                        self.uri.clone(),
                        "html".to_owned(),
                        self.version,
                        self.text.clone(),
                    ),
                },
            ),
        }
    }

    fn close(&mut self, editor: &Editor) {
        for (id, _) in self.synced.drain() {
            if let Some(server) = editor.language_server_by_id(id) {
                server.text_document_did_close(lsp::TextDocumentIdentifier::new(self.uri.clone()));
            }
        }
        self.looked_for_server = false;
    }
}

/// Why a forwarded request got no answer from an HTML language server.
enum Unanswered {
    /// There is no HTML language server to ask.
    NoServer,
    /// The server or the generated document is not ready; a later request
    /// may be answered.
    NotYet,
}

#[derive(Default)]
pub struct HtmlBridge {
    /// The generated HTML documents, by the URI of their Razor document.
    documents: HashMap<lsp::Url, HtmlDocument>,
    /// The HTML language server in use.
    server: Option<LanguageServerId>,
}

impl HtmlBridge {
    /// Answers a request from a language server if it is part of the Razor
    /// HTML protocol. Any other request is handed back untouched.
    pub fn handle_call(
        &mut self,
        editor: &mut Editor,
        server_id: LanguageServerId,
        method: &str,
        id: &jsonrpc::Id,
        params: jsonrpc::Params,
    ) -> Option<jsonrpc::Params> {
        if method != UPDATE_HTML && !is_forwarded_request(&params) {
            return Some(params);
        }
        let Some(server) = editor.language_servers.get_by_id(server_id).cloned() else {
            return Some(params);
        };

        let id = id.clone();
        if method == UPDATE_HTML {
            match params.parse::<UpdateHtml>() {
                Ok(update) => self.update(editor, update),
                Err(err) => log::error!("Malformed {UPDATE_HTML} request: {err}"),
            }
            reply(&server, id, Value::Null);
            return None;
        }

        let pending = match params.parse::<ForwardedRequest>() {
            Ok(forwarded) => self.forward(editor, &server, method, forwarded),
            Err(err) => {
                log::error!("Malformed forwarded {method} request: {err}");
                Err(Unanswered::NoServer)
            }
        };
        match pending {
            Ok(response) => {
                let method = method.to_owned();
                tokio::spawn(async move {
                    let result = match tokio::time::timeout(FORWARD_TIMEOUT, response).await {
                        // The HTML server has nothing to say here.
                        Ok(Ok(Value::Null)) => no_answer(&method, false),
                        Ok(Ok(result)) => result,
                        Ok(Err(err)) => {
                            log::warn!("HTML language server failed to answer {method}: {err}");
                            no_answer(&method, true)
                        }
                        Err(_) => {
                            log::warn!("HTML language server did not answer {method} in time");
                            no_answer(&method, true)
                        }
                    };
                    reply(&server, id, result);
                });
            }
            Err(Unanswered::NotYet) => reply(&server, id, no_answer(method, true)),
            Err(Unanswered::NoServer) => reply(&server, id, no_answer(method, false)),
        }
        None
    }

    fn update(&mut self, editor: &mut Editor, update: UpdateHtml) {
        let razor_uri = update.text_document.uri;
        self.close_orphans(editor, &razor_uri);

        let Some(uri) = html_uri(&razor_uri) else {
            return;
        };
        let document = self
            .documents
            .entry(razor_uri.clone())
            .or_insert_with(|| HtmlDocument {
                uri,
                checksum: String::new(),
                text: String::new(),
                version: 0,
                synced: HashMap::new(),
                looked_for_server: false,
            });
        document.checksum = update.checksum;
        document.text = update.text;
        document.version += 1;

        // Like the servers of an ordinary document, the HTML server is looked
        // for when a Razor document is opened and not on every request: one
        // that keeps exiting would otherwise be started over and over.
        let mut server = self
            .server
            .and_then(|id| editor.language_servers.get_by_id(id).cloned());
        if server.is_none() && !std::mem::replace(&mut document.looked_for_server, true) {
            server = html_server(editor, &razor_uri);
        }
        self.server = server.map(|server| server.id());
    }

    fn forward(
        &mut self,
        editor: &Editor,
        razor_server: &Client,
        method: &str,
        forwarded: ForwardedRequest,
    ) -> Result<impl Future<Output = helix_lsp::Result<Value>>, Unanswered> {
        let server = self
            .server
            .and_then(|id| editor.language_servers.get_by_id(id))
            .ok_or(Unanswered::NoServer)?;
        // Notifications to a server that is still starting are dropped, so
        // the generated document cannot be opened on it yet.
        if !server.is_initialized() {
            return Err(Unanswered::NotYet);
        }
        // Positions are passed on as they are, so both servers have to count
        // columns the same way.
        if server.offset_encoding() != razor_server.offset_encoding() {
            return Err(Unanswered::NoServer);
        }
        let document = self
            .documents
            .get_mut(&forwarded.text_document.uri)
            .filter(|document| document.checksum == forwarded.checksum)
            .ok_or(Unanswered::NotYet)?;
        document.sync(server);

        let mut request = forwarded.request;
        retarget(&mut request, &document.uri);
        Ok(server.call_raw(method.to_owned(), request))
    }

    /// Closes the generated documents whose Razor document is no longer open.
    ///
    /// Their text is kept: the Razor server does not send it again when the
    /// document is reopened unchanged.
    fn close_orphans(&mut self, editor: &Editor, keep: &lsp::Url) {
        for (razor_uri, document) in &mut self.documents {
            let closed = document.synced.is_empty() && !document.looked_for_server;
            let open = razor_uri == keep
                || razor_uri
                    .to_file_path()
                    .is_ok_and(|path| editor.document_by_path(path).is_some());
            if !closed && !open {
                document.close(editor);
            }
        }
    }
}

/// Whether the URI names HTML generated from a Razor document. What an HTML
/// language server reports about such a document concerns no file.
pub fn is_generated_html(uri: &lsp::Url) -> bool {
    uri.path().ends_with(HTML_SUFFIX)
}

fn reply(server: &Client, id: jsonrpc::Id, result: Value) {
    if let Err(err) = server.reply(id.clone(), Ok(result)) {
        log::error!(
            "Failed to send reply to server '{}' request {id}: {err}",
            server.name()
        );
    }
}

/// What to answer when no HTML language server does.
///
/// The server drops its own completions (components, directive attributes)
/// when the HTML ones are missing, so that the client asks again. An empty
/// list keeps them, and marking it incomplete asks for another request when
/// one may fare better. Everywhere else, null stands for an empty HTML part.
fn no_answer(method: &str, retry: bool) -> Value {
    use lsp::request::Request as _;
    if method == lsp::request::Completion::METHOD {
        serde_json::json!({ "isIncomplete": retry, "items": [] })
    } else {
        Value::Null
    }
}

/// Whether the parameters are the envelope around a request that the server
/// wants answered from the generated HTML. No request that the protocol
/// itself sends from server to client has this shape.
fn is_forwarded_request(params: &jsonrpc::Params) -> bool {
    matches!(params, jsonrpc::Params::Map(map)
        if map.get("checksum").is_some_and(Value::is_string)
            && map.contains_key("request")
            && map.get("textDocument").is_some_and(|document| document.get("uri").is_some()))
}

fn html_uri(razor_uri: &lsp::Url) -> Option<lsp::Url> {
    lsp::Url::parse(&format!("{razor_uri}{HTML_SUFFIX}")).ok()
}

/// Points the forwarded request at the generated HTML document. Positions
/// need no translation, since both documents have the same layout.
fn retarget(request: &mut Value, html_uri: &lsp::Url) {
    if let Some(uri) = request.pointer_mut("/textDocument/uri") {
        *uri = Value::String(html_uri.to_string());
    }
}

/// The first language server configured for `html` that can be started.
fn html_server(editor: &mut Editor, razor_uri: &lsp::Url) -> Option<Arc<Client>> {
    let config = editor.config();
    if !config.lsp.enable {
        return None;
    }
    let loader = editor.syn_loader.load();
    let html = loader
        .language_configs()
        .find(|language| language.language_id == "html")?;
    let path = razor_uri.to_file_path().ok();
    let server = editor
        .language_servers
        .get(
            html,
            path.as_ref(),
            &config.workspace_lsp_roots,
            config.lsp.snippets,
        )
        .find_map(|(_, server)| server.ok());
    server
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn params(value: Value) -> jsonrpc::Params {
        match value {
            Value::Object(map) => jsonrpc::Params::Map(map),
            Value::Array(values) => jsonrpc::Params::Array(values),
            _ => jsonrpc::Params::None,
        }
    }

    #[test]
    fn recognizes_the_envelope_of_a_forwarded_request() {
        let forwarded = params(json!({
            "textDocument": { "uri": "file:///app/Counter.razor" },
            "checksum": "abc",
            "request": {
                "textDocument": { "uri": "file:///app/Counter.razor" },
                "position": { "line": 3, "character": 4 },
            },
        }));
        assert!(is_forwarded_request(&forwarded));
    }

    #[test]
    fn leaves_ordinary_server_requests_alone() {
        // workspace/configuration
        assert!(!is_forwarded_request(&params(
            json!({ "items": [{ "section": "html" }] })
        )));
        // workspace/applyEdit
        assert!(!is_forwarded_request(&params(
            json!({ "label": "Rename", "edit": { "changes": {} } })
        )));
        // window/showDocument names a URI, but neither a checksum nor a request.
        assert!(!is_forwarded_request(&params(
            json!({ "uri": "file:///app/Counter.razor" })
        )));
        assert!(!is_forwarded_request(&jsonrpc::Params::None));
    }

    #[test]
    fn keeps_the_servers_own_completions_without_an_html_server() {
        assert_eq!(
            no_answer("textDocument/completion", false),
            json!({ "isIncomplete": false, "items": [] })
        );
        assert_eq!(
            no_answer("textDocument/completion", true),
            json!({ "isIncomplete": true, "items": [] })
        );
        assert_eq!(no_answer("textDocument/hover", true), Value::Null);
        assert_eq!(no_answer("textDocument/formatting", false), Value::Null);
    }

    #[test]
    fn tells_generated_documents_from_files() {
        let razor = lsp::Url::parse("file:///app/Counter.razor").unwrap();
        assert!(is_generated_html(&html_uri(&razor).unwrap()));
        assert!(!is_generated_html(&razor));
        assert!(!is_generated_html(
            &lsp::Url::parse("file:///app/wwwroot/index.html").unwrap()
        ));
    }

    #[test]
    fn names_the_generated_document_after_the_razor_file() {
        let razor = lsp::Url::parse("file:///app/Pages/My%20Page.razor").unwrap();
        assert_eq!(
            html_uri(&razor).unwrap().as_str(),
            "file:///app/Pages/My%20Page.razor__virtual.html"
        );
    }

    #[test]
    fn retargets_the_request_and_keeps_the_position() {
        let html = lsp::Url::parse("file:///app/Counter.razor__virtual.html").unwrap();
        let mut request = json!({
            "textDocument": { "uri": "file:///app/Counter.razor" },
            "position": { "line": 3, "character": 4 },
        });
        retarget(&mut request, &html);
        assert_eq!(
            request,
            json!({
                "textDocument": { "uri": "file:///app/Counter.razor__virtual.html" },
                "position": { "line": 3, "character": 4 },
            })
        );
    }

    #[test]
    fn leaves_a_request_without_a_document_as_it_is() {
        // completionItem/resolve carries the item itself.
        let html = lsp::Url::parse("file:///app/Counter.razor__virtual.html").unwrap();
        let mut request = json!({ "label": "div", "data": { "id": 7 } });
        retarget(&mut request, &html);
        assert_eq!(request, json!({ "label": "div", "data": { "id": 7 } }));
    }
}
