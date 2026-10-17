((comment) @injection.content
 (#set! injection.language "comment"))

; SQL in any string, whatever library or function receives it. A string is
; SQL when its text starts with an explicit marker, `--sql` or `/* sql */`
; (a SQL comment, so it highlights as one), or with a statement written in
; upper case: SELECT ... FROM, INSERT INTO, UPDATE ... SET, DELETE FROM,
; WITH ... AS (, CREATE/ALTER/DROP TABLE and the like. Lower-case SQL needs
; the marker. In f-strings the interpolations are left out of the SQL, and an
; implicitly concatenated string counts as one when its first part qualifies.
(string
  (string_start)
  .
  (string_content) @_sql @injection.content
  (#match? @_sql "^\\s*(--\\s*sql\\b|/\\*\\s*sql\\s*\\*/|SELECT\\s(?s:.*?)\\sFROM\\s|SELECT\\s[\\w\\s.,*()]*$|INSERT\\s+INTO\\s|UPDATE\\s+\\S+\\s+SET\\s|DELETE\\s+FROM\\s|WITH\\s+(RECURSIVE\\s+)?\\w+(?s:.*?)\\sAS\\s*\\(|(CREATE|ALTER|DROP)\\s+(OR\\s+REPLACE\\s+)?(TEMP(ORARY)?\\s+)?(UNIQUE\\s+)?(TABLE|VIEW|INDEX|TRIGGER|SCHEMA)\\s)")
  [
    (string_content) @injection.content
    (interpolation)
  ]*
  .
  (string_end)
  (#set! injection.language "sql"))

; ("SELECT id " "FROM users")
(concatenated_string
  .
  (string
    (string_start)
    .
    (string_content) @_sql @injection.content
    (#match? @_sql "^\\s*(--\\s*sql\\b|/\\*\\s*sql\\s*\\*/|SELECT\\s(?s:.*?)\\sFROM\\s|SELECT\\s[\\w\\s.,*()]*$|INSERT\\s+INTO\\s|UPDATE\\s+\\S+\\s+SET\\s|DELETE\\s+FROM\\s|WITH\\s+(RECURSIVE\\s+)?\\w+(?s:.*?)\\sAS\\s*\\(|(CREATE|ALTER|DROP)\\s+(OR\\s+REPLACE\\s+)?(TEMP(ORARY)?\\s+)?(UNIQUE\\s+)?(TABLE|VIEW|INDEX|TRIGGER|SCHEMA)\\s)")
    [
      (string_content) @injection.content
      (interpolation)
    ]*
    .
    (string_end))
  (string
    (string_content) @injection.content)+
  (#set! injection.language "sql"))

; Match all 9 functions in the `re` module from the standard library that
; that takes a regex pattern as first argument.
; https://docs.python.org/3/library/re.html#functions
(call
  function: (attribute
    object: (identifier) @_module (#eq? @_module "re")
    attribute: (identifier) @_function (#any-of? @_function "compile" "search" "match" "fullmatch" "sub" "subn" "findall" "finditer" "split"))
  arguments: (argument_list
    . (string
        (string_content) @injection.content))
  (#set! injection.language "regex"))
