//! Seed script security validation.
//!
//! This module provides security checks for seed scripts before execution,
//! including detection of potentially dangerous SQL operations and validation
//! of SQL syntax.

use std::path::Path;

use crate::error::LifecycleError;

/// Dangerous SQL keywords that should not appear in seed scripts.
const DANGEROUS_KEYWORDS: &[&str] = &[
    "DROP DATABASE",
    "CREATE DATABASE",
    "ALTER DATABASE",
    "GRANT",
    "REVOKE",
    "CREATE ROLE",
    "ALTER ROLE",
    "DROP ROLE",
    "CREATE USER",
    "ALTER USER",
    "DROP USER",
    "TRUNCATE",
];

/// SQL injection patterns to detect in seed scripts.
const INJECTION_PATTERNS: &[&str] = &[
    "--",  // SQL comment
    "/*",  // Block comment start
    "*/",  // Block comment end
    ";--", // Comment after statement
    "UNION SELECT",
    "UNION ALL SELECT",
    "EXEC(",
    "EXECUTE(",
    "XP_", // Extended procedures
];

/// Security validation result for a seed script.
#[derive(Debug, Clone)]
pub struct SeedSecurityReport {
    /// Whether the seed script passed all security checks.
    pub is_safe: bool,
    /// List of security warnings found.
    pub warnings: Vec<SecurityWarning>,
    /// List of critical issues that block execution.
    pub errors: Vec<SecurityError>,
}

/// A security warning (non-blocking).
#[derive(Debug, Clone)]
pub struct SecurityWarning {
    pub line: usize,
    pub message: String,
    pub suggestion: String,
}

/// A critical security error (blocks execution).
#[derive(Debug, Clone)]
pub struct SecurityError {
    pub line: usize,
    pub message: String,
}

impl SeedSecurityReport {
    /// Create a new empty report.
    pub fn new() -> Self {
        Self {
            is_safe: true,
            warnings: Vec::new(),
            errors: Vec::new(),
        }
    }

    /// Add a warning to the report.
    pub fn add_warning(&mut self, line: usize, message: String, suggestion: String) {
        self.warnings.push(SecurityWarning {
            line,
            message,
            suggestion,
        });
    }

    /// Add an error to the report (makes it unsafe).
    pub fn add_error(&mut self, line: usize, message: String) {
        self.is_safe = false;
        self.errors.push(SecurityError { line, message });
    }
}

impl Default for SeedSecurityReport {
    fn default() -> Self {
        Self::new()
    }
}

/// Validate a seed script for security issues.
///
/// # Security Checks
///
/// 1. **Dangerous Keywords**: Detects DROP DATABASE, CREATE USER, etc.
/// 2. **Injection Patterns**: Detects SQL comments, UNION SELECT, etc.
/// 3. **File Path Validation**: Ensures seed file is within allowed directory.
///
/// # Example
///
/// ```rust,no_run
/// use sdkwork_database_lifecycle::seed_security::validate_seed_script;
/// use std::path::Path;
///
/// let report = validate_seed_script(Path::new("seeds/001_init.sql")).unwrap();
/// if report.is_safe {
///     // Safe to execute
/// } else {
///     // Handle security issues
///     for error in &report.errors {
///         eprintln!("Security error at line {}: {}", error.line, error.message);
///     }
/// }
/// ```
pub fn validate_seed_script(seed_path: &Path) -> Result<SeedSecurityReport, LifecycleError> {
    let content = std::fs::read_to_string(seed_path)
        .map_err(|e| LifecycleError::Seed(format!("failed to read seed file: {}", e)))?;

    validate_seed_content(&content, seed_path)
}

/// Validate seed script content directly.
pub fn validate_seed_content(
    content: &str,
    seed_path: &Path,
) -> Result<SeedSecurityReport, LifecycleError> {
    let mut report = SeedSecurityReport::new();

    // Check 1: Dangerous keywords
    check_dangerous_keywords(content, &mut report);

    // Check 2: Injection patterns
    check_injection_patterns(content, &mut report);

    // Check 3: File path validation
    check_file_path(seed_path, &mut report);

    // Check 4: Statement validation
    check_statement_balance(content, &mut report);

    Ok(report)
}

/// Check for dangerous SQL keywords.
fn check_dangerous_keywords(content: &str, report: &mut SeedSecurityReport) {
    for (line_num, line) in content.lines().enumerate() {
        let Some(sql_line) = executable_sql_prefix(line) else {
            continue;
        };
        let tokens = sql_tokens(&sql_line);

        for keyword in DANGEROUS_KEYWORDS {
            if contains_keyword_phrase(&tokens, keyword) {
                report.add_error(
                    line_num + 1,
                    format!(
                        "Dangerous operation '{}' detected in seed script. \
                         Seed scripts should only contain data insertion/modification, \
                         not schema or permission changes.",
                        keyword
                    ),
                );
            }
        }

        // Warn about potentially destructive operations
        if contains_keyword_phrase(&tokens, "DROP TABLE")
            || contains_keyword_phrase(&tokens, "DROP INDEX")
        {
            report.add_warning(
                line_num + 1,
                format!("DROP operation detected: {}", line.trim()),
                "Consider using conditional drops (IF EXISTS) or ensuring tables are empty first."
                    .to_string(),
            );
        }
    }
}

/// Check for SQL injection patterns.
fn check_injection_patterns(content: &str, report: &mut SeedSecurityReport) {
    for (line_num, line) in content.lines().enumerate() {
        if is_full_line_comment(line) {
            continue;
        }

        let masked_line = mask_string_literals(line);
        let upper_line = masked_line.to_uppercase();
        let tokens = sql_tokens(&upper_line);

        for pattern in INJECTION_PATTERNS {
            let matched = match *pattern {
                "--" | ";--" => contains_line_comment(&upper_line),
                "/*" | "*/" => upper_line.contains(pattern),
                "UNION SELECT" | "UNION ALL SELECT" => contains_keyword_phrase(&tokens, pattern),
                _ => upper_line.contains(pattern),
            };
            if matched {
                report.add_error(
                    line_num + 1,
                    format!(
                        "Potential SQL injection pattern '{}' detected. \
                         Seed scripts should not contain SQL comments or dynamic SQL patterns.",
                        pattern
                    ),
                );
            }
        }
    }
}

fn is_full_line_comment(line: &str) -> bool {
    line.trim_start().starts_with("--")
}

fn executable_sql_prefix(line: &str) -> Option<String> {
    if is_full_line_comment(line) {
        return None;
    }

    let masked = mask_string_literals(line);
    let prefix_len = masked.find("--").unwrap_or(masked.len());
    let prefix = &masked[..prefix_len];
    Some(prefix.to_uppercase())
}

fn mask_string_literals(line: &str) -> String {
    let mut output = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
    let mut in_string = false;
    let mut string_char = '\0';

    while let Some(ch) = chars.next() {
        if in_string {
            output.push(' ');
            if ch == string_char {
                if matches!(chars.peek(), Some(next) if *next == string_char) {
                    output.push(' ');
                    chars.next();
                } else {
                    in_string = false;
                }
            }
            continue;
        }

        if ch == '\'' || ch == '"' {
            in_string = true;
            string_char = ch;
            output.push(' ');
        } else {
            output.push(ch);
        }
    }

    output
}

fn sql_tokens(line: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();

    for ch in line.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            current.push(ch.to_ascii_uppercase());
        } else if !current.is_empty() {
            tokens.push(std::mem::take(&mut current));
        }
    }

    if !current.is_empty() {
        tokens.push(current);
    }

    tokens
}

fn contains_keyword_phrase(tokens: &[String], phrase: &str) -> bool {
    let phrase_tokens = sql_tokens(phrase);
    if phrase_tokens.is_empty() || tokens.len() < phrase_tokens.len() {
        return false;
    }

    tokens
        .windows(phrase_tokens.len())
        .any(|window| window == phrase_tokens.as_slice())
}

fn contains_line_comment(masked_upper_line: &str) -> bool {
    masked_upper_line.contains("--")
}

/// Validate that the seed file path is safe.
fn check_file_path(path: &Path, report: &mut SeedSecurityReport) {
    let path_str = path.to_string_lossy();

    // Check for path traversal
    if path_str.contains("..") {
        report.add_error(
            0,
            "Path traversal detected in seed file path. Seed files must be within the seeds directory."
                .to_string(),
        );
    }

    // Check for suspicious file extensions
    if let Some(ext) = path.extension() {
        let ext_str = ext.to_string_lossy().to_lowercase();
        if ext_str != "sql" {
            report.add_warning(
                0,
                format!("Unexpected file extension '.{}'", ext_str),
                "Seed files should have .sql extension.".to_string(),
            );
        }
    }
}

/// Check for balanced statements (basic SQL validation).
///
/// Line comments (`-- …`) are skipped: a SQL engine ignores them entirely, so
/// prose apostrophes (e.g. a song title `Don't` in a catalog comment) must not
/// flip the string scanner and poison the paren balance of the whole file.
fn check_statement_balance(content: &str, report: &mut SeedSecurityReport) {
    let mut paren_depth = 0i32;
    let mut in_string = false;
    let mut string_char = ' ';

    for (line_num, line) in content.lines().enumerate() {
        let mut prev_char = ' ';
        let mut in_line_comment = false;
        let mut chars = line.chars().peekable();

        while let Some(ch) = chars.next() {
            if !in_string && !in_line_comment && ch == '-' && chars.peek() == Some(&'-') {
                in_line_comment = true;
                continue;
            }
            if in_line_comment {
                continue;
            }

            // Handle string literals
            if (ch == '\'' || ch == '"') && prev_char != '\\' {
                if !in_string {
                    in_string = true;
                    string_char = ch;
                } else if ch == string_char {
                    in_string = false;
                }
            }

            // Track parentheses outside strings
            if !in_string {
                if ch == '(' {
                    paren_depth += 1;
                } else if ch == ')' {
                    paren_depth -= 1;
                    if paren_depth < 0 {
                        report.add_error(
                            line_num + 1,
                            "Unbalanced parentheses: closing ')' without matching '('.".to_string(),
                        );
                        paren_depth = 0;
                    }
                }
            }

            prev_char = ch;
        }
    }

    if paren_depth > 0 {
        report.add_error(
            content.lines().count(),
            format!(
                "Unbalanced parentheses: {} unclosed '(' in seed script.",
                paren_depth
            ),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn test_safe_seed_content() {
        let content = "INSERT INTO users (id, name) VALUES (1, 'Alice');";
        let report = validate_seed_content(content, &PathBuf::from("seeds/test.sql")).unwrap();
        assert!(report.is_safe);
        assert!(report.errors.is_empty());
    }

    #[test]
    fn test_dangerous_drop_database() {
        let content = "DROP DATABASE mydb;";
        let report = validate_seed_content(content, &PathBuf::from("seeds/test.sql")).unwrap();
        assert!(!report.is_safe);
        assert!(!report.errors.is_empty());
    }

    #[test]
    fn test_injection_pattern_detected() {
        let content = "SELECT * FROM users; -- malicious comment";
        let report = validate_seed_content(content, &PathBuf::from("seeds/test.sql")).unwrap();
        assert!(!report.is_safe);
    }

    #[test]
    fn test_grant_substrings_in_seed_identifiers_are_safe() {
        let content = r#"
INSERT INTO entitlement_grant (
    id, grant_no, grant_policy, granted_quantity, total_granted
) VALUES (
    'seed-entitlement-grant-user-1', 'seed-grant-user-1', 'membership_plan', '20', '20'
);
"#;
        let report = validate_seed_content(content, &PathBuf::from("seeds/test.sql")).unwrap();
        assert!(report.is_safe, "{:?}", report.errors);
    }

    #[test]
    fn test_full_line_seed_comments_are_safe() {
        let content = r#"
-- Language-neutral bootstrap rows for local development.
INSERT INTO users (id, name) VALUES (1, 'Alice');
"#;
        let report = validate_seed_content(content, &PathBuf::from("seeds/test.sql")).unwrap();
        assert!(report.is_safe, "{:?}", report.errors);
    }

    #[test]
    fn test_path_traversal_detected() {
        let content = "INSERT INTO test VALUES (1);";
        let report =
            validate_seed_content(content, &PathBuf::from("../../etc/passwd.sql")).unwrap();
        assert!(!report.is_safe);
    }

    #[test]
    fn test_unbalanced_parentheses() {
        let content = "INSERT INTO test (id VALUES (1);";
        let report = validate_seed_content(content, &PathBuf::from("seeds/test.sql")).unwrap();
        assert!(!report.is_safe);
    }

    #[test]
    fn test_line_comments_do_not_poison_the_balance_scan() {
        // A prose apostrophe inside a line comment must not open a phantom
        // string and desync the paren balance of the rest of the file.
        let content = concat!(
            "-- ── Course: 【We Don't Talk Anymore】 tutorial ──\n",
            "-- note (half-width parens in prose are fine)\n",
            "INSERT INTO test (id, name) VALUES (1, 'it''s fine');\n",
        );
        let report = validate_seed_content(content, &PathBuf::from("seeds/test.sql")).unwrap();
        assert!(report.is_safe, "errors: {:?}", report.errors);
    }

    #[test]
    fn test_warning_for_drop_table() {
        let content = "DROP TABLE IF EXISTS test;";
        let report = validate_seed_content(content, &PathBuf::from("seeds/test.sql")).unwrap();
        assert!(report.is_safe); // Warning, not error
        assert!(!report.warnings.is_empty());
    }
}
