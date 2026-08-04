//! Puppet parser plugin — full-parse mode.
//!
//! Handles `.pp` files (Puppet manifests).
//! The plugin parses source with Tree-sitter inside Rust/Wasm.

use intentdiff_plugin_sdk::{
    cst::CstNode,
    hash::structural_hash_with_memo,
    tree::{SemanticNode, SemanticNodeBuilder},
};

wit_bindgen::generate!({
    path: "wit/plugin.wit",
    world: "parser-plugin",
});

use crate::exports::intentdiff::plugin::parser::ExamplePair;
use crate::exports::intentdiff::plugin::parser::Guest;
use crate::exports::intentdiff::plugin::parser::LanguageInfoRecord;
use crate::exports::intentdiff::plugin::parser::ParserMode;

const PLUGIN_METADATA: &str = include_str!("../plugin_metadata.info");

fn language_info_for(ids: Vec<String>) -> Vec<LanguageInfoRecord> {
    let metadata = intentdiff_plugin_sdk::metadata::parse_plugin_metadata(PLUGIN_METADATA);
    ids.into_iter()
        .map(|language_id| {
            let info = metadata.language_or_default(&language_id);
            LanguageInfoRecord {
                language_id: info.language_id,
                language_name: info.language_name,
                language_short_name: info.language_short_name,
                monaco_language: info.monaco_language,
                default_filename: info.default_filename,
                language_file_extensions: info.language_file_extensions,
                author: metadata.author().to_string(),
                plugin_version: metadata.plugin_version().to_string(),
                last_updated: metadata.last_updated().to_string(),
            }
        })
        .collect()
}
struct PuppetParser;

const TRIVIA: &[&str] = &["comment", "whitespace", "#"];

const SEMANTIC_TYPES: &[&str] = &[
    // Root
    "source_file",
    "manifest",
    // Top-level declarations
    "class_definition",
    "defined_type",
    "node_statement",
    "node_definition",
    // Resources
    "resource_statement",
    "resource_declaration",
    "resource_body",
    "attribute",
    "attribute_list",
    // Relationships
    "relationship_statement",
    "relationship",
    // Function calls
    "function_call",
    "call_expression",
    "include_statement",
    "require_statement",
    "contain_statement",
    // Parameters
    "parameter",
    "parameter_list",
    // Variables and assignments
    "variable",
    "assignment_statement",
    "variable_expression",
    // Control flow
    "if_statement",
    "elsif_clause",
    "else_clause",
    "unless_statement",
    "case_statement",
    "case_matcher",
    "default_case",
    // Loops
    "each_statement",
    "map_statement",
    "filter_statement",
    "reduce_statement",
    // Misc
    "selector",
    "string",
    "heredoc",
    "regular_expression",
    "hash",
    "array",
    "type",
    "resource_reference",
    "resource_collector",
    "class_include",
    "class_require",
    "class_contain",
];

fn is_semantic(node_type: &str) -> bool {
    SEMANTIC_TYPES.contains(&node_type)
}

fn label_for(node: &CstNode) -> String {
    if node.is_leaf() {
        return node.text_or_empty().to_string();
    }
    // Literal containers label with their captured source text (SDK-shared, issue #47).
    if let Some(label) = intentdiff_plugin_sdk::ts_convert::literal_label(node) {
        return label;
    }
    match node.node_type.as_str() {
        "class_definition" | "defined_type" => {
            for child in &node.children {
                if matches!(
                    child.node_type.as_str(),
                    "identifier" | "name" | "class_name" | "type_name"
                ) {
                    return child.text_or_empty().to_string();
                }
            }
        }
        "node_statement" | "node_definition" => {
            // Label is the node spec (string, regexp, or "default")
            for child in &node.children {
                if matches!(
                    child.node_type.as_str(),
                    "string" | "regular_expression" | "default" | "node_name"
                ) {
                    return child.text_or_empty().to_string();
                }
            }
        }
        "resource_statement" | "resource_declaration" => {
            // "file { '/etc/hosts': }" → "file /etc/hosts"
            let mut type_name = String::new();
            let mut title = String::new();
            for child in &node.children {
                match child.node_type.as_str() {
                    "identifier" | "resource_type" | "name" if type_name.is_empty() => {
                        type_name = child.text_or_empty().to_string();
                    }
                    "resource_body" => {
                        for grandchild in &child.children {
                            if matches!(
                                grandchild.node_type.as_str(),
                                "string" | "expression" | "title"
                            ) {
                                title = grandchild.text_or_empty().to_string();
                                break;
                            }
                        }
                    }
                    "string" | "title" if title.is_empty() => {
                        title = child.text_or_empty().to_string();
                    }
                    _ => {}
                }
            }
            if !type_name.is_empty() && !title.is_empty() {
                // Titles are IDENTITY, not literals: strip the quotes the (now
                // content-preserving, issue #46) string capture includes, so the
                // resource label stays `file /tmp/x` rather than `file '/tmp/x'`.
                let title_clean = title.trim().trim_matches(|c| c == '\'' || c == '"');
                return format!("{} {}", type_name, title_clean);
            }
            if !type_name.is_empty() {
                return type_name;
            }
        }
        "function_call" | "call_expression" => {
            for child in &node.children {
                if matches!(
                    child.node_type.as_str(),
                    "identifier" | "name" | "function_name"
                ) {
                    return child.text_or_empty().to_string();
                }
            }
        }
        "include_statement" | "class_include" => {
            for child in &node.children {
                if matches!(
                    child.node_type.as_str(),
                    "class_name" | "identifier" | "name" | "string"
                ) {
                    return child.text_or_empty().to_string();
                }
            }
        }
        "require_statement" | "class_require" | "contain_statement" | "class_contain" => {
            for child in &node.children {
                if matches!(
                    child.node_type.as_str(),
                    "class_name" | "identifier" | "name" | "string"
                ) {
                    return child.text_or_empty().to_string();
                }
            }
        }
        "parameter" => {
            for child in &node.children {
                if matches!(child.node_type.as_str(), "variable" | "identifier" | "name") {
                    return child.text_or_empty().to_string();
                }
            }
        }
        "attribute" => {
            if let Some(first) = node.children.first() {
                return first.text_or_empty().to_string();
            }
        }
        _ => {}
    }
    for child in &node.children {
        if matches!(child.node_type.as_str(), "identifier" | "name") {
            return child.text_or_empty().to_string();
        }
    }
    node.node_type.clone()
}

fn is_class_like(node_type: &str) -> bool {
    matches!(
        node_type,
        "class_definition" | "defined_type" | "node_statement" | "node_definition"
    )
}

fn is_method_like(_node_type: &str) -> bool {
    false // Puppet has no method concept
}

fn convert(
    node: &CstNode,
    id_prefix: &str,
    parent_class: Option<&str>,
    memo: &mut std::collections::HashMap<usize, String>,
) -> Option<SemanticNode> {
    // Class context threads for descendants but never sets parent_type here
    // (no method-like nodes in this grammar's review model).
    convert_semantic_classed(
        node,
        id_prefix,
        parent_class,
        memo,
        &|t| TRIVIA.contains(&t),
        &is_semantic,
        &is_class_like,
        &|_| false,
        &label_for,
    )
}



use intentdiff_plugin_sdk::ts_convert::{convert_semantic_classed, node_to_cst};

fn parse_source(source: &str) -> Result<CstNode, String> {
    let mut parser = tree_sitter::Parser::new();
    let lang = tree_sitter_puppet::LANGUAGE.into();
    parser
        .set_language(&lang)
        .map_err(|_| "Failed to load puppet grammar".to_string())?;
    let tree = parser
        .parse(source, None)
        .ok_or_else(|| "Parse failed".to_string())?;
    Ok(node_to_cst(tree.root_node(), source.as_bytes()))
}

fn process_impl(source: &str) -> String {
    let root: CstNode = match parse_source(source) {
        Ok(n) => n,
        Err(e) => return format!(r#"{{\"error\":\"{}\"}}"#, e),
    };
    let mut memo: std::collections::HashMap<usize, String> = std::collections::HashMap::new();
    let sem = match convert(&root, "0", None, &mut memo) {
        Some(n) => n,
        None => return r#"{"error":"Empty semantic tree"}"#.to_string(),
    };
    match serde_json::to_string(&sem) {
        Ok(s) => s,
        Err(e) => format!(r#"{{"error":"Serialisation error: {}"}}"#, e),
    }
}

impl Guest for PuppetParser {
    fn get_parser_mode() -> ParserMode {
        ParserMode::FullParse
    }
    fn grammar_id() -> String {
        "puppet".to_string()
    }
    fn detect_language(filename: String, _content: String) -> String {
        if filename.to_lowercase().ends_with(".pp") {
            return "puppet".to_string();
        }
        String::new()
    }
    fn preprocess_source(source: String) -> String {
        source
    }
    fn process(input: String, _language: String, _filename: String) -> String {
        process_impl(&input)
    }
    fn trivia_node_types() -> Vec<String> {
        TRIVIA.iter().map(|s| s.to_string()).collect()
    }
    fn language_ids() -> Vec<String> {
        vec!["puppet".to_string()]
    }
    fn language_info() -> Vec<LanguageInfoRecord> {
        language_info_for(Self::language_ids())
    }
    fn priority() -> i32 {
        0
    }

    fn example(_language: String) -> ExamplePair {
        ExamplePair {
            old: "class greeting {\n  notify { 'hello':\n    message => 'Hello, World!',\n  }\n}\n".to_string(),
            new: "class greeting (\n  String $message = 'Hello, World!',\n  String $target   = 'console',\n) {\n  notify { 'hello':\n    message => $message,\n  }\n\n  file { '/tmp/greeting.txt':\n    ensure  => present,\n    content => $message,\n  }\n}\n".to_string(),
        }
    }
}
export!(PuppetParser);

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exports::intentdiff::plugin::parser::Guest;
    use intentdiff_plugin_sdk::testing as t;

    #[test]
    fn grammar_id_nonempty() {
        assert!(!PuppetParser::grammar_id().is_empty());
    }

    #[test]
    fn language_ids_contain_grammar_id() {
        let gid = PuppetParser::grammar_id();
        let ids = PuppetParser::language_ids();
        assert!(
            ids.contains(&gid),
            "language_ids {:?} must contain {:?}",
            ids,
            gid
        );
    }

    #[test]
    fn detect_language_known_ext() {
        let r = PuppetParser::detect_language("test.pp".to_string(), "".to_string());
        assert_eq!(r.as_str(), "puppet");
    }

    #[test]
    fn detect_language_unknown_ext() {
        let r =
            PuppetParser::detect_language("test.xyz_notareal_ext_9z8y".to_string(), "".to_string());
        assert_eq!(r.as_str(), "");
    }

    #[test]
    fn parser_mode_is_full_parse() {
        assert!(matches!(
            PuppetParser::get_parser_mode(),
            ParserMode::FullParse
        ));
    }

    #[test]
    fn process_impl_accepts_raw_example_source() {
        let example = PuppetParser::example(PuppetParser::grammar_id());
        let out = process_impl(&example.old);
        t::assert_valid_json(&out, "process(raw example)");
        assert!(!out.contains("\"error\""), "{out}");
    }
    #[test]
    fn process_impl_empty_returns_valid_json() {
        let out = process_impl("");
        t::assert_valid_json(&out, "process(empty)");
    }

    #[test]
    fn process_impl_whitespace_returns_valid_json() {
        let out = process_impl("   \n  ");
        t::assert_valid_json(&out, "process(whitespace)");
    }
}
