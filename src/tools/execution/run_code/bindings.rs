//! Portable binding names for the programmatic-tool-calling runtime (issue
//! #134, borrowed from DSH `ptc-runtime`).
//!
//! A binding is a tool exposed to the model-authored program under a function
//! name. The name must be legal in **every** backend we might execute on, so
//! the rules are the union of the target languages' keywords plus the globals
//! the bootstrap itself defines. Validation happens *before* a process is
//! spawned: an unusable binding name must not consume a run.

use std::collections::BTreeSet;
use std::fmt;

/// Reserved words of ECMAScript (including strict-mode and future-reserved)
/// unioned with Python's keywords. A binding name in this set is rejected by
/// at least one backend, so it is rejected everywhere.
pub const PORTABLE_RESERVED_WORDS: &[&str] = &[
    // ECMAScript keywords + future reserved words.
    "abstract",
    "await",
    "boolean",
    "break",
    "byte",
    "case",
    "catch",
    "char",
    "class",
    "const",
    "continue",
    "debugger",
    "default",
    "delete",
    "do",
    "double",
    "else",
    "enum",
    "export",
    "extends",
    "false",
    "final",
    "finally",
    "float",
    "for",
    "function",
    "goto",
    "if",
    "implements",
    "import",
    "in",
    "instanceof",
    "int",
    "interface",
    "let",
    "long",
    "native",
    "new",
    "null",
    "package",
    "private",
    "protected",
    "public",
    "return",
    "short",
    "static",
    "super",
    "switch",
    "synchronized",
    "this",
    "throw",
    "throws",
    "transient",
    "true",
    "try",
    "typeof",
    "var",
    "void",
    "volatile",
    "while",
    "with",
    "yield",
    // Python keywords + soft keywords that read as keywords in context.
    "and",
    "as",
    "assert",
    "async",
    "def",
    "del",
    "elif",
    "except",
    "from",
    "global",
    "is",
    "lambda",
    "match",
    "nonlocal",
    "not",
    "or",
    "pass",
    "raise",
    "None",
    "True",
    "False",
];

/// Names the bootstrap (or the host runtime) provides in the program's scope.
/// A binding that shadows one of these would silently break the bridge.
pub const RESERVED_BINDING_GLOBALS: &[&str] = &[
    "console",
    "process",
    "require",
    "module",
    "exports",
    "globalThis",
    "global",
    "__dirname",
    "__filename",
    "arguments",
    "eval",
    "binding",
    "bindings",
    "pending",
    "protocolWrite",
];

/// Members the bootstrap assigns on the error object it throws for a failed
/// tool call. A binding name may never collide with them.
pub const RESERVED_ERROR_MEMBERS: &[&str] = &["name", "message", "stack", "cause"];

/// Why a binding name was rejected. Reported before a run so the caller never
/// pays for a process it cannot use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BindingNameError {
    /// The name is empty.
    Empty,
    /// The name is not a portable identifier (`[A-Za-z_][A-Za-z0-9_]*`).
    NotAnIdentifier(String),
    /// The name is a reserved word in ECMAScript and/or Python.
    ReservedWord(String),
    /// The name shadows a global the bootstrap relies on.
    ReservedGlobal(String),
    /// The same name was registered twice.
    Duplicate(String),
}

impl fmt::Display for BindingNameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "binding name must not be empty"),
            Self::NotAnIdentifier(name) => write!(
                f,
                "binding name {name:?} is not a portable identifier \
                 ([A-Za-z_][A-Za-z0-9_]* — `$` is not portable)"
            ),
            Self::ReservedWord(name) => {
                write!(f, "binding name {name:?} is a reserved word")
            }
            Self::ReservedGlobal(name) => write!(
                f,
                "binding name {name:?} is reserved by the run_code runtime"
            ),
            Self::Duplicate(name) => write!(f, "binding name {name:?} is registered twice"),
        }
    }
}

impl std::error::Error for BindingNameError {}

/// `[A-Za-z_][A-Za-z0-9_]*` — the intersection of ECMAScript and Python
/// identifiers. Deliberately excludes `$` (legal in JS, illegal in Python) so
/// one bindings list stays valid for every backend.
fn is_portable_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Validate a single binding name against the portable rules.
pub fn validate_binding_name(name: &str) -> Result<(), BindingNameError> {
    if name.is_empty() {
        return Err(BindingNameError::Empty);
    }
    if !is_portable_identifier(name) {
        return Err(BindingNameError::NotAnIdentifier(name.to_string()));
    }
    if PORTABLE_RESERVED_WORDS.contains(&name) {
        return Err(BindingNameError::ReservedWord(name.to_string()));
    }
    if RESERVED_BINDING_GLOBALS.contains(&name) || RESERVED_ERROR_MEMBERS.contains(&name) {
        return Err(BindingNameError::ReservedGlobal(name.to_string()));
    }
    Ok(())
}

/// The set of tool bindings exposed to a program: one function per registered
/// tool, named exactly like the tool.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BindingTable {
    names: Vec<String>,
}

impl BindingTable {
    /// Build the table from the registered tool names, validating every name.
    /// A single bad name rejects the whole table — the run never starts.
    pub fn from_tool_names<I>(names: I) -> Result<Self, BindingNameError>
    where
        I: IntoIterator,
        I::Item: Into<String>,
    {
        let mut seen = BTreeSet::new();
        let mut collected = Vec::new();
        for raw in names {
            let name = raw.into();
            validate_binding_name(&name)?;
            if !seen.insert(name.clone()) {
                return Err(BindingNameError::Duplicate(name));
            }
            collected.push(name);
        }
        collected.sort();
        Ok(Self { names: collected })
    }

    /// An empty table (a program with no tool bindings).
    pub fn empty() -> Self {
        Self { names: Vec::new() }
    }

    pub fn names(&self) -> &[String] {
        &self.names
    }

    pub fn is_empty(&self) -> bool {
        self.names.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_style_names_are_accepted() {
        let table = BindingTable::from_tool_names(["Read", "Write", "Bash", "read_file"])
            .expect("valid names");
        assert_eq!(table.names(), &["Bash", "Read", "Write", "read_file"]);
    }

    #[test]
    fn reserved_words_are_rejected_before_any_run() {
        for name in ["for", "class", "await", "def", "lambda", "yield"] {
            assert_eq!(
                validate_binding_name(name),
                Err(BindingNameError::ReservedWord(name.to_string())),
                "{name} must be rejected"
            );
        }
    }

    #[test]
    fn runtime_globals_and_error_members_are_rejected() {
        for name in ["console", "process", "require", "binding"] {
            assert_eq!(
                validate_binding_name(name),
                Err(BindingNameError::ReservedGlobal(name.to_string())),
                "{name} must be rejected"
            );
        }
        for name in RESERVED_ERROR_MEMBERS {
            assert_eq!(
                validate_binding_name(name),
                Err(BindingNameError::ReservedGlobal((*name).to_string())),
                "{name} must be rejected"
            );
        }
    }

    #[test]
    fn non_portable_identifiers_are_rejected() {
        for name in ["", "has-dash", "1leading", "with$dollar", "with.dot", "süß"] {
            assert!(
                validate_binding_name(name).is_err(),
                "{name:?} must be rejected"
            );
        }
    }

    /// Acceptance: an illegal binding name is rejected before a run — the
    /// table never gets built, so no process is ever spawned.
    #[test]
    fn one_bad_name_rejects_the_whole_table() {
        let err = BindingTable::from_tool_names(["Read", "for"]).expect_err("must reject");
        assert_eq!(err, BindingNameError::ReservedWord("for".to_string()));
    }

    #[test]
    fn duplicates_are_rejected() {
        let err =
            BindingTable::from_tool_names(["Read", "Read"]).expect_err("must reject duplicates");
        assert_eq!(err, BindingNameError::Duplicate("Read".to_string()));
    }
}
