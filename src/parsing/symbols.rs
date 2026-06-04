use serde::{Deserialize, Serialize};

/// Classification of a named symbol extracted from source code.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum SymbolKind {
    Function,
    Method,
    Struct,
    Trait,
    Impl,
    Class,
    Module,
    Interface,
}

/// Fully-qualified reference to a symbol — uniquely identifies it across files.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct QualifiedSymbol {
    /// Absolute path of the source file.
    pub file: String,
    /// Containing scopes from outermost to innermost (e.g. `["MyClass", "impl"]`).
    pub scope_path: Vec<String>,
    /// The symbol's own name.
    pub name: String,
}

impl QualifiedSymbol {
    /// Produce a fully-qualified name: `file::scope1::scope2::name`.
    pub fn fqn(&self) -> String {
        let mut parts = vec![self.file.clone()];
        parts.extend(self.scope_path.iter().cloned());
        parts.push(self.name.clone());
        parts.join("::")
    }

    /// SurrealDB record ID string: `symbol:⟨fqn⟩`.
    pub fn record_id(&self) -> String {
        format!("symbol:⟨{}⟩", self.fqn())
    }
}

/// A symbol extracted from a source file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Symbol {
    pub qualified: QualifiedSymbol,
    pub kind: SymbolKind,
    pub line_start: u32,
    pub line_end: u32,
    pub signature: Option<String>,
    /// FQN of the enclosing symbol, if any.
    pub parent_fqn: Option<String>,
}
