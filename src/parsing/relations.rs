use crate::parsing::symbols::QualifiedSymbol;

/// The target of an edge — either fully resolved to a `QualifiedSymbol`,
/// or unresolved (we know the name but not which file defines it).
#[derive(Debug, Clone)]
pub enum EdgeTarget {
    Resolved(QualifiedSymbol),
    Unresolved {
        name: String,
        import_path: Option<String>,
        qualifier: Option<String>,
    },
}

/// Which kind of relationship an edge represents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EdgeKind {
    Calls,
    Uses,
    Imports,
    Contains,
    Implements,
}

/// A raw directed edge produced during parsing.
#[derive(Debug, Clone)]
pub struct RawEdge {
    pub from: QualifiedSymbol,
    pub to: EdgeTarget,
    pub kind: EdgeKind,
    /// Source line where the relationship occurs.
    pub line: u32,
}
