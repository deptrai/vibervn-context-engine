pub mod chunker;
pub mod relations;
pub mod symbols;

use std::collections::HashMap;
use std::path::Path;

use tracing::{warn};
use tree_sitter::{Node, Parser};

use crate::parsing::chunker::{Chunk, chunk_file};
use crate::parsing::relations::{EdgeKind, EdgeTarget, RawEdge};
use crate::parsing::symbols::{QualifiedSymbol, Symbol, SymbolKind};

/// Result of parsing one source file.
#[derive(Debug)]
pub struct ParseResult {
    pub symbols: Vec<Symbol>,
    pub edges: Vec<RawEdge>,
    pub chunks: Vec<Chunk>,
    /// Import map: local name → source file path (best-effort, only for resolved imports).
    pub imports: HashMap<String, String>,
}

// ─── Language detection ───────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    Python,
    JavaScript,
    TypeScript,
    Tsx,
    Rust,
    Go,
    Java,
    Other,
}

pub fn detect_language(path: &Path) -> Lang {
    match path.extension().and_then(|e| e.to_str()) {
        Some("py") => Lang::Python,
        Some("js" | "jsx" | "mjs" | "cjs") => Lang::JavaScript,
        Some("ts") => Lang::TypeScript,
        Some("tsx") => Lang::Tsx,
        Some("rs") => Lang::Rust,
        Some("go") => Lang::Go,
        Some("java") => Lang::Java,
        _ => Lang::Other,
    }
}

// ─── Entry point ──────────────────────────────────────────────────────────

/// Parse a source file and return symbols, edges, and chunks.
/// Falls back to coverage-only chunks on parse failure.
pub fn parse_file(file_path: &str, source: &str) -> ParseResult {
    let path = Path::new(file_path);
    let lang = detect_language(path);

    let (symbols, edges) = match lang {
        Lang::Python => parse_with_tree_sitter(
            file_path,
            source,
            tree_sitter_python::LANGUAGE.into(),
            extract_python,
        ),
        Lang::JavaScript | Lang::Tsx => parse_with_tree_sitter(
            file_path,
            source,
            tree_sitter_javascript::LANGUAGE.into(),
            extract_javascript,
        ),
        Lang::TypeScript => parse_with_tree_sitter(
            file_path,
            source,
            tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            extract_typescript,
        ),
        Lang::Rust => parse_with_tree_sitter(
            file_path,
            source,
            tree_sitter_rust::LANGUAGE.into(),
            extract_rust,
        ),
        Lang::Go => parse_with_tree_sitter(
            file_path,
            source,
            tree_sitter_go::LANGUAGE.into(),
            extract_go,
        ),
        Lang::Java => parse_with_tree_sitter(
            file_path,
            source,
            tree_sitter_java::LANGUAGE.into(),
            extract_java,
        ),
        Lang::Other => (vec![], vec![]),
    };

    let chunks = chunk_file(file_path, source, &symbols);

    ParseResult {
        symbols,
        edges,
        chunks,
        imports: HashMap::new(),
    }
}

// ─── Generic tree-sitter driver ───────────────────────────────────────────

fn parse_with_tree_sitter<F>(
    file_path: &str,
    source: &str,
    language: tree_sitter::Language,
    extractor: F,
) -> (Vec<Symbol>, Vec<RawEdge>)
where
    F: Fn(&str, &str, &tree_sitter::Tree) -> (Vec<Symbol>, Vec<RawEdge>),
{
    let mut parser = Parser::new();
    if let Err(e) = parser.set_language(&language) {
        warn!(file = file_path, error = %e, "failed to set tree-sitter language");
        return (vec![], vec![]);
    }
    match parser.parse(source, None) {
        Some(tree) => extractor(file_path, source, &tree),
        None => {
            warn!(file = file_path, "tree-sitter parse returned None");
            (vec![], vec![])
        }
    }
}

// ─── Utility helpers ──────────────────────────────────────────────────────

fn node_text<'a>(node: &Node, source: &'a str) -> &'a str {
    node.utf8_text(source.as_bytes()).unwrap_or("")
}

fn node_line_start(node: &Node) -> u32 {
    node.start_position().row as u32 + 1
}

fn node_line_end(node: &Node) -> u32 {
    node.end_position().row as u32 + 1
}

fn make_symbol(
    file: &str,
    name: &str,
    scope_path: Vec<String>,
    kind: SymbolKind,
    line_start: u32,
    line_end: u32,
    signature: Option<String>,
    parent_fqn: Option<String>,
) -> Symbol {
    Symbol {
        qualified: QualifiedSymbol {
            file: file.to_string(),
            scope_path,
            name: name.to_string(),
        },
        kind,
        line_start,
        line_end,
        signature,
        parent_fqn,
    }
}

// ─── Python extractor ─────────────────────────────────────────────────────

fn extract_python(file: &str, source: &str, tree: &tree_sitter::Tree) -> (Vec<Symbol>, Vec<RawEdge>) {
    let mut symbols = Vec::new();
    let mut edges = Vec::new();
    let root = tree.root_node();
    extract_python_node(file, source, &root, &[], None, &mut symbols, &mut edges);
    (symbols, edges)
}

fn extract_python_node(
    file: &str,
    source: &str,
    node: &Node,
    scope: &[String],
    parent_fqn: Option<&str>,
    symbols: &mut Vec<Symbol>,
    edges: &mut Vec<RawEdge>,
) {
    match node.kind() {
        "function_definition" | "async_function_definition" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(&name_node, source).to_string();
                let kind = if !scope.is_empty() { SymbolKind::Method } else { SymbolKind::Function };
                let sym = make_symbol(
                    file, &name, scope.to_vec(), kind,
                    node_line_start(node), node_line_end(node),
                    None, parent_fqn.map(|s| s.to_string()),
                );
                let fqn = sym.qualified.fqn();
                symbols.push(sym);

                let mut child_scope = scope.to_vec();
                child_scope.push(name);
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    extract_python_node(file, source, &child, &child_scope, Some(&fqn), symbols, edges);
                }
            }
        }
        "class_definition" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(&name_node, source).to_string();
                let sym = make_symbol(
                    file, &name, scope.to_vec(), SymbolKind::Class,
                    node_line_start(node), node_line_end(node),
                    None, parent_fqn.map(|s| s.to_string()),
                );
                let fqn = sym.qualified.fqn();
                symbols.push(sym);

                let mut child_scope = scope.to_vec();
                child_scope.push(name);
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    extract_python_node(file, source, &child, &child_scope, Some(&fqn), symbols, edges);
                }
            }
        }
        "call" => {
            if let Some(func_node) = node.child_by_field_name("function") {
                let callee_name = node_text(&func_node, source).to_string();
                if let Some(from_sym) = scope_to_qualified(file, scope) {
                    edges.push(RawEdge {
                        from: from_sym,
                        to: EdgeTarget::Unresolved {
                            name: callee_name,
                            import_path: None,
                            qualifier: None,
                        },
                        kind: EdgeKind::Calls,
                        line: node_line_start(node),
                    });
                }
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    extract_python_node(file, source, &child, scope, parent_fqn, symbols, edges);
                }
            } else {
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    extract_python_node(file, source, &child, scope, parent_fqn, symbols, edges);
                }
            }
        }
        _ => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                extract_python_node(file, source, &child, scope, parent_fqn, symbols, edges);
            }
        }
    }
}

fn scope_to_qualified(file: &str, scope: &[String]) -> Option<QualifiedSymbol> {
    scope.last().map(|name| QualifiedSymbol {
        file: file.to_string(),
        scope_path: scope[..scope.len() - 1].to_vec(),
        name: name.clone(),
    })
}

// ─── JavaScript extractor ─────────────────────────────────────────────────

fn extract_javascript(file: &str, source: &str, tree: &tree_sitter::Tree) -> (Vec<Symbol>, Vec<RawEdge>) {
    let mut symbols = Vec::new();
    let mut edges = Vec::new();
    let root = tree.root_node();
    extract_js_node(file, source, &root, &[], None, &mut symbols, &mut edges);
    (symbols, edges)
}

fn extract_js_node(
    file: &str,
    source: &str,
    node: &Node,
    scope: &[String],
    parent_fqn: Option<&str>,
    symbols: &mut Vec<Symbol>,
    edges: &mut Vec<RawEdge>,
) {
    match node.kind() {
        "function_declaration" | "function" => {
            let name = node.child_by_field_name("name")
                .map(|n| node_text(&n, source).to_string())
                .unwrap_or_else(|| "<anonymous>".to_string());
            let kind = if !scope.is_empty() { SymbolKind::Method } else { SymbolKind::Function };
            let sym = make_symbol(
                file, &name, scope.to_vec(), kind,
                node_line_start(node), node_line_end(node),
                None, parent_fqn.map(|s| s.to_string()),
            );
            let fqn = sym.qualified.fqn();
            symbols.push(sym);
            let mut child_scope = scope.to_vec();
            child_scope.push(name);
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                extract_js_node(file, source, &child, &child_scope, Some(&fqn), symbols, edges);
            }
        }
        "class_declaration" | "class" => {
            let name = node.child_by_field_name("name")
                .map(|n| node_text(&n, source).to_string())
                .unwrap_or_else(|| "<anonymous>".to_string());
            let sym = make_symbol(
                file, &name, scope.to_vec(), SymbolKind::Class,
                node_line_start(node), node_line_end(node),
                None, parent_fqn.map(|s| s.to_string()),
            );
            let fqn = sym.qualified.fqn();
            symbols.push(sym);
            let mut child_scope = scope.to_vec();
            child_scope.push(name);
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                extract_js_node(file, source, &child, &child_scope, Some(&fqn), symbols, edges);
            }
        }
        "call_expression" => {
            if let Some(func_node) = node.child_by_field_name("function") {
                let callee_name = node_text(&func_node, source).to_string();
                if let Some(from_sym) = scope_to_qualified(file, scope) {
                    edges.push(RawEdge {
                        from: from_sym,
                        to: EdgeTarget::Unresolved {
                            name: callee_name,
                            import_path: None,
                            qualifier: None,
                        },
                        kind: EdgeKind::Calls,
                        line: node_line_start(node),
                    });
                }
            }
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                extract_js_node(file, source, &child, scope, parent_fqn, symbols, edges);
            }
        }
        _ => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                extract_js_node(file, source, &child, scope, parent_fqn, symbols, edges);
            }
        }
    }
}

// ─── TypeScript extractor ─────────────────────────────────────────────────

fn extract_typescript(file: &str, source: &str, tree: &tree_sitter::Tree) -> (Vec<Symbol>, Vec<RawEdge>) {
    // TypeScript grammar is a superset of JS grammar — reuse JS extractor.
    extract_javascript(file, source, tree)
}

// ─── Rust extractor ───────────────────────────────────────────────────────

fn extract_rust(file: &str, source: &str, tree: &tree_sitter::Tree) -> (Vec<Symbol>, Vec<RawEdge>) {
    let mut symbols = Vec::new();
    let mut edges = Vec::new();
    let root = tree.root_node();
    extract_rust_node(file, source, &root, &[], None, &mut symbols, &mut edges);
    (symbols, edges)
}

fn extract_rust_node(
    file: &str,
    source: &str,
    node: &Node,
    scope: &[String],
    parent_fqn: Option<&str>,
    symbols: &mut Vec<Symbol>,
    edges: &mut Vec<RawEdge>,
) {
    match node.kind() {
        "function_item" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(&name_node, source).to_string();
                let kind = if scope.iter().any(|s| s.starts_with("impl")) {
                    SymbolKind::Method
                } else {
                    SymbolKind::Function
                };
                let sym = make_symbol(
                    file, &name, scope.to_vec(), kind,
                    node_line_start(node), node_line_end(node),
                    None, parent_fqn.map(|s| s.to_string()),
                );
                let fqn = sym.qualified.fqn();
                symbols.push(sym);
                let mut child_scope = scope.to_vec();
                child_scope.push(name);
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    extract_rust_node(file, source, &child, &child_scope, Some(&fqn), symbols, edges);
                }
            }
        }
        "struct_item" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(&name_node, source).to_string();
                let sym = make_symbol(
                    file, &name, scope.to_vec(), SymbolKind::Struct,
                    node_line_start(node), node_line_end(node),
                    None, parent_fqn.map(|s| s.to_string()),
                );
                symbols.push(sym);
            }
        }
        "trait_item" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(&name_node, source).to_string();
                let sym = make_symbol(
                    file, &name, scope.to_vec(), SymbolKind::Trait,
                    node_line_start(node), node_line_end(node),
                    None, parent_fqn.map(|s| s.to_string()),
                );
                let fqn = sym.qualified.fqn();
                symbols.push(sym);
                let mut child_scope = scope.to_vec();
                child_scope.push(name);
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    extract_rust_node(file, source, &child, &child_scope, Some(&fqn), symbols, edges);
                }
            }
        }
        "impl_item" => {
            let type_name = node.child_by_field_name("type")
                .map(|n| node_text(&n, source).to_string())
                .unwrap_or_else(|| "impl".to_string());
            let impl_name = format!("impl_{}", type_name);
            let sym = make_symbol(
                file, &impl_name, scope.to_vec(), SymbolKind::Impl,
                node_line_start(node), node_line_end(node),
                None, parent_fqn.map(|s| s.to_string()),
            );
            let fqn = sym.qualified.fqn();
            symbols.push(sym);
            let mut child_scope = scope.to_vec();
            child_scope.push(impl_name);
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                extract_rust_node(file, source, &child, &child_scope, Some(&fqn), symbols, edges);
            }
        }
        "mod_item" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(&name_node, source).to_string();
                let sym = make_symbol(
                    file, &name, scope.to_vec(), SymbolKind::Module,
                    node_line_start(node), node_line_end(node),
                    None, parent_fqn.map(|s| s.to_string()),
                );
                let fqn = sym.qualified.fqn();
                symbols.push(sym);
                let mut child_scope = scope.to_vec();
                child_scope.push(name);
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    extract_rust_node(file, source, &child, &child_scope, Some(&fqn), symbols, edges);
                }
            }
        }
        "call_expression" => {
            if let Some(func_node) = node.child_by_field_name("function") {
                let callee_name = node_text(&func_node, source).to_string();
                if let Some(from_sym) = scope_to_qualified(file, scope) {
                    edges.push(RawEdge {
                        from: from_sym,
                        to: EdgeTarget::Unresolved {
                            name: callee_name,
                            import_path: None,
                            qualifier: None,
                        },
                        kind: EdgeKind::Calls,
                        line: node_line_start(node),
                    });
                }
            }
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                extract_rust_node(file, source, &child, scope, parent_fqn, symbols, edges);
            }
        }
        _ => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                extract_rust_node(file, source, &child, scope, parent_fqn, symbols, edges);
            }
        }
    }
}

// ─── Go extractor ─────────────────────────────────────────────────────────

fn extract_go(file: &str, source: &str, tree: &tree_sitter::Tree) -> (Vec<Symbol>, Vec<RawEdge>) {
    let mut symbols = Vec::new();
    let mut edges = Vec::new();
    let root = tree.root_node();
    extract_go_node(file, source, &root, &[], None, &mut symbols, &mut edges);
    (symbols, edges)
}

fn extract_go_node(
    file: &str,
    source: &str,
    node: &Node,
    scope: &[String],
    parent_fqn: Option<&str>,
    symbols: &mut Vec<Symbol>,
    edges: &mut Vec<RawEdge>,
) {
    match node.kind() {
        "function_declaration" | "method_declaration" => {
            let name = node.child_by_field_name("name")
                .map(|n| node_text(&n, source).to_string())
                .unwrap_or_else(|| "<anon>".to_string());
            let kind = if node.kind() == "method_declaration" {
                SymbolKind::Method
            } else {
                SymbolKind::Function
            };
            let sym = make_symbol(
                file, &name, scope.to_vec(), kind,
                node_line_start(node), node_line_end(node),
                None, parent_fqn.map(|s| s.to_string()),
            );
            let fqn = sym.qualified.fqn();
            symbols.push(sym);
            let mut child_scope = scope.to_vec();
            child_scope.push(name);
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                extract_go_node(file, source, &child, &child_scope, Some(&fqn), symbols, edges);
            }
        }
        "type_declaration" => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() == "type_spec" {
                    if let Some(name_node) = child.child_by_field_name("name") {
                        let name = node_text(&name_node, source).to_string();
                        let sym = make_symbol(
                            file, &name, scope.to_vec(), SymbolKind::Struct,
                            node_line_start(&child), node_line_end(&child),
                            None, parent_fqn.map(|s| s.to_string()),
                        );
                        symbols.push(sym);
                    }
                }
            }
        }
        "call_expression" => {
            if let Some(func_node) = node.child_by_field_name("function") {
                let callee_name = node_text(&func_node, source).to_string();
                if let Some(from_sym) = scope_to_qualified(file, scope) {
                    edges.push(RawEdge {
                        from: from_sym,
                        to: EdgeTarget::Unresolved {
                            name: callee_name,
                            import_path: None,
                            qualifier: None,
                        },
                        kind: EdgeKind::Calls,
                        line: node_line_start(node),
                    });
                }
            }
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                extract_go_node(file, source, &child, scope, parent_fqn, symbols, edges);
            }
        }
        _ => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                extract_go_node(file, source, &child, scope, parent_fqn, symbols, edges);
            }
        }
    }
}

// ─── Java extractor ───────────────────────────────────────────────────────

fn extract_java(file: &str, source: &str, tree: &tree_sitter::Tree) -> (Vec<Symbol>, Vec<RawEdge>) {
    let mut symbols = Vec::new();
    let mut edges = Vec::new();
    let root = tree.root_node();
    extract_java_node(file, source, &root, &[], None, &mut symbols, &mut edges);
    (symbols, edges)
}

fn extract_java_node(
    file: &str,
    source: &str,
    node: &Node,
    scope: &[String],
    parent_fqn: Option<&str>,
    symbols: &mut Vec<Symbol>,
    edges: &mut Vec<RawEdge>,
) {
    match node.kind() {
        "class_declaration" | "interface_declaration" => {
            let kind = if node.kind() == "interface_declaration" {
                SymbolKind::Interface
            } else {
                SymbolKind::Class
            };
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(&name_node, source).to_string();
                let sym = make_symbol(
                    file, &name, scope.to_vec(), kind,
                    node_line_start(node), node_line_end(node),
                    None, parent_fqn.map(|s| s.to_string()),
                );
                let fqn = sym.qualified.fqn();
                symbols.push(sym);
                let mut child_scope = scope.to_vec();
                child_scope.push(name);
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    extract_java_node(file, source, &child, &child_scope, Some(&fqn), symbols, edges);
                }
            }
        }
        "method_declaration" | "constructor_declaration" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(&name_node, source).to_string();
                let sym = make_symbol(
                    file, &name, scope.to_vec(), SymbolKind::Method,
                    node_line_start(node), node_line_end(node),
                    None, parent_fqn.map(|s| s.to_string()),
                );
                let fqn = sym.qualified.fqn();
                symbols.push(sym);
                let mut child_scope = scope.to_vec();
                child_scope.push(name);
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    extract_java_node(file, source, &child, &child_scope, Some(&fqn), symbols, edges);
                }
            }
        }
        "method_invocation" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let callee_name = node_text(&name_node, source).to_string();
                if let Some(from_sym) = scope_to_qualified(file, scope) {
                    edges.push(RawEdge {
                        from: from_sym,
                        to: EdgeTarget::Unresolved {
                            name: callee_name,
                            import_path: None,
                            qualifier: None,
                        },
                        kind: EdgeKind::Calls,
                        line: node_line_start(node),
                    });
                }
            }
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                extract_java_node(file, source, &child, scope, parent_fqn, symbols, edges);
            }
        }
        _ => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                extract_java_node(file, source, &child, scope, parent_fqn, symbols, edges);
            }
        }
    }
}
