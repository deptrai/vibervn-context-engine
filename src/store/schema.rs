/// SurrealDB DDL — executed once at startup to bootstrap all tables, fields, and indexes.
pub const SCHEMA_DDL: &str = r#"
DEFINE TABLE symbol SCHEMAFULL;
DEFINE FIELD name       ON symbol TYPE string;
DEFINE FIELD kind       ON symbol TYPE string;
DEFINE FIELD file       ON symbol TYPE string;
DEFINE FIELD line_start ON symbol TYPE int;
DEFINE FIELD line_end   ON symbol TYPE int;
DEFINE FIELD signature  ON symbol TYPE option<string>;
DEFINE FIELD parent     ON symbol TYPE option<record<symbol>>;
DEFINE INDEX idx_symbol_file ON symbol FIELDS file;
DEFINE INDEX idx_symbol_name ON symbol FIELDS name;

DEFINE TABLE chunk SCHEMAFULL;
DEFINE FIELD file       ON chunk TYPE string;
DEFINE FIELD line_start ON chunk TYPE int;
DEFINE FIELD line_end   ON chunk TYPE int;
DEFINE FIELD content    ON chunk TYPE string;
DEFINE FIELD embedding  ON chunk TYPE array<float>;
DEFINE FIELD symbol_ref ON chunk TYPE option<record<symbol>>;
DEFINE INDEX idx_chunk_file ON chunk FIELDS file;

DEFINE TABLE calls TYPE RELATION IN symbol OUT symbol;
DEFINE FIELD line     ON calls TYPE int;
DEFINE FIELD in_file  ON calls TYPE string;
DEFINE FIELD out_file ON calls TYPE string;
DEFINE INDEX idx_calls_in_file ON calls FIELDS in_file;
DEFINE INDEX idx_calls_out_file ON calls FIELDS out_file;

DEFINE TABLE uses TYPE RELATION IN symbol OUT symbol;
DEFINE FIELD in_file  ON uses TYPE string;
DEFINE FIELD out_file ON uses TYPE string;
DEFINE INDEX idx_uses_in_file ON uses FIELDS in_file;
DEFINE INDEX idx_uses_out_file ON uses FIELDS out_file;

DEFINE TABLE imports TYPE RELATION IN symbol OUT symbol;
DEFINE FIELD in_file  ON imports TYPE string;
DEFINE FIELD out_file ON imports TYPE string;
DEFINE INDEX idx_imports_in_file ON imports FIELDS in_file;
DEFINE INDEX idx_imports_out_file ON imports FIELDS out_file;

DEFINE TABLE contains TYPE RELATION IN symbol OUT symbol;
DEFINE FIELD in_file  ON contains TYPE string;
DEFINE FIELD out_file ON contains TYPE string;
DEFINE INDEX idx_contains_in_file ON contains FIELDS in_file;
DEFINE INDEX idx_contains_out_file ON contains FIELDS out_file;

DEFINE TABLE implements TYPE RELATION IN symbol OUT symbol;
DEFINE FIELD in_file  ON implements TYPE string;
DEFINE FIELD out_file ON implements TYPE string;
DEFINE INDEX idx_implements_in_file ON implements FIELDS in_file;
DEFINE INDEX idx_implements_out_file ON implements FIELDS out_file;

DEFINE TABLE file_meta SCHEMAFULL;
DEFINE FIELD path  ON file_meta TYPE string;
DEFINE FIELD mtime ON file_meta TYPE int;
DEFINE FIELD size  ON file_meta TYPE int;
DEFINE FIELD repo  ON file_meta TYPE string;
DEFINE INDEX idx_filemeta_path ON file_meta FIELDS path UNIQUE;

DEFINE TABLE index_meta SCHEMAFULL;
DEFINE FIELD key   ON index_meta TYPE string;
DEFINE FIELD value ON index_meta TYPE string;
DEFINE INDEX idx_meta_key ON index_meta FIELDS key UNIQUE;
"#;
