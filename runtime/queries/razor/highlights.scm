; inherits: c-sharp

(tag_name) @tag

; A component, as opposed to an HTML element.
((tag_name) @constructor
  (#match? @constructor "^[A-Z]"))

(erroneous_end_tag) @error

(doctype) @constant

(attribute_name) @attribute

(quoted_attribute_value) @string

(entity) @string.special.symbol

(element
  [
    "<"
    ">"
    "</"
    "/>"
  ] @punctuation.bracket)

(element
  "=" @punctuation.delimiter)

(razor_html_attribute
  "=" @punctuation.delimiter)

[
  (razor_comment)
  (html_comment)
] @comment.block

[
  "at_page"
  "at_using"
  "at_model"
  "at_rendermode"
  "at_inject"
  "at_implements"
  "at_layout"
  "at_inherits"
  "at_attribute"
  "at_typeparam"
  "at_namespace"
  "at_preservewhitespace"
  "at_addtaghelper"
  "at_removetaghelper"
  "at_taghelperprefix"
  "at_block"
] @keyword.directive

(taghelper_wildcard) @constant.character

[
  "at_at_escape"
  "at_colon_transition"
] @constant.character.escape

[
  "at_lock"
  "at_section"
] @keyword

[
  "at_if"
  "at_switch"
] @keyword.control.conditional

[
  "at_for"
  "at_foreach"
  "at_while"
  "at_do"
] @keyword.control.repeat

[
  "at_try"
  "catch"
  "finally"
] @keyword.control.exception

[
  "at_implicit"
  "at_explicit"
] @punctuation.special

(razor_rendermode) @constant.builtin

(razor_attribute_name) @attribute
