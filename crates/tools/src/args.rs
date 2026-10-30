//! Reading a tool call's parameters, and refusing one that is outside its domain.
//!
//! A JSON Schema says `{"type": "integer"}` and nothing more, so a parameter's real domain
//! exists only in the code that consumes it — and the failures below are all one shape: an
//! out-of-domain value read as **a different value** rather than as a caller error.
//!
//! - `read` with `line_limit = -3` sliced a 10-line file **from the end** and then printed a
//!   continuation hint naming a line number that cannot be asked for.
//! - `read` with `line_limit = 0` was falsy, so it was read as *absent* and returned the
//!   default cap.
//! - `grep` with `context_before = -2` printed the match header and **no line at all**.
//! - `edit` with `replace_all = "false"` — a truthy string — replaced **both** occurrences,
//!   editing a place the caller had explicitly excluded.
//!
//! Rust removes the third and fourth mechanically (`-2` is not a `usize`, a string is not a
//! `bool`), and keeps the first two reachable: `as_u64().unwrap_or(default)` turns `-3` into the
//! default and `0` into a value the consumer may or may not honour. So the readers here refuse
//! anything that is not the type, name the parameter and the value, and never substitute.
//!
//! Presence is `is not null`, never "truthy": `0` is a value.

use serde_json::Value;

/// The first spelling present among `names`, with the spelling used.
///
/// A parameter that is explicitly `null` is absent, because that is what a model that means
/// "unset" sends. `false` and `0` are **values**, and are returned as such.
fn present<'a, 'n>(args: &'a Value, names: &'n [&'n str]) -> Option<(&'a Value, &'n str)> {
    let object = args.as_object()?;
    for name in names {
        if let Some(value) = object.get(*name) {
            if !value.is_null() {
                // Borrowed from the call site's own list: a refusal names the spelling the
                // caller used, and doing that must not allocate a copy per call.
                return Some((value, name));
            }
        }
    }
    None
}

/// Read an integer at least `minimum`, or hand back the refusal it earns.
///
/// Returns `Ok(default)` when the parameter is absent, so a caller can write one line and get
/// the documented default. A boolean is refused: `true` is not a count, whatever its numeric
/// value in a looser language.
pub fn count_argument(
    args: &Value,
    names: &[&str],
    minimum: u64,
    default: Option<u64>,
) -> Result<Option<u64>, String> {
    let Some((raw, spelled)) = present(args, names) else {
        return Ok(default);
    };
    let refuse = |what: &str| {
        Err(format!(
            "{spelled} must be {what} (got {raw}); this call is refused rather than answered \
             with a different reading."
        ))
    };
    if raw.is_boolean() {
        return refuse(&format!(
            "an integer >= {minimum}; a boolean is not a count"
        ));
    }
    let Some(number) = raw.as_u64() else {
        // A float that is integral is a count a JSON writer may have produced; a fractional one
        // is not a count in any reading.
        if let Some(float) = raw.as_f64() {
            if float.is_finite() && float.fract() != 0.0 {
                return refuse(&format!(
                    "an integer >= {minimum}; a fractional count is not"
                ));
            }
            if float.is_finite() && float >= 0.0 {
                let value = float as u64;
                return if value >= minimum {
                    Ok(Some(value))
                } else {
                    refuse(&format!("an integer >= {minimum}"))
                };
            }
        }
        return refuse(&format!("an integer >= {minimum}"));
    };
    if number < minimum {
        return refuse(&format!("an integer >= {minimum}"));
    }
    Ok(Some(number))
}

/// Read a boolean, or hand back the refusal it earns.
///
/// The two string spellings are read as the value they name, because a model that quotes a
/// boolean means the boolean. **Anything else is refused** rather than coerced, and the reason
/// is not stylistic: this flag decides how many places an edit writes to, so a value read as its
/// opposite writes where the caller said not to.
pub fn boolean_argument(args: &Value, names: &[&str], default: bool) -> Result<bool, String> {
    let Some((raw, spelled)) = present(args, names) else {
        return Ok(default);
    };
    if let Some(flag) = raw.as_bool() {
        return Ok(flag);
    }
    if let Some(text) = raw.as_str() {
        match text.trim().to_ascii_lowercase().as_str() {
            "true" => return Ok(true),
            "false" => return Ok(false),
            _ => {}
        }
    }
    Err(format!(
        "{spelled} must be true or false (got {raw}); this call is refused rather than run with \
         a value read as its opposite."
    ))
}

/// Read a required non-empty string, or hand back the refusal it earns.
pub fn string_argument(args: &Value, names: &[&str]) -> Result<String, String> {
    match present(args, names) {
        Some((raw, spelled)) => match raw.as_str() {
            Some(text) => Ok(text.to_string()),
            None => Err(format!("{spelled} must be a string (got {raw})")),
        },
        // The spellings are joined rather than naming a default: with two accepted spellings
        // there is no one name to blame, and a model has to be told which ones it may use.
        None => Err(format!("{} is required and was not given", names.join("/"))),
    }
}

/// Read an optional string, returning `None` when absent.
pub fn optional_string_argument(args: &Value, names: &[&str]) -> Result<Option<String>, String> {
    let Some((raw, spelled)) = present(args, names) else {
        return Ok(None);
    };
    match raw.as_str() {
        Some(text) => Ok(Some(text.to_string())),
        None => Err(format!("{spelled} must be a string (got {raw})")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn an_absent_parameter_takes_its_default() {
        let args = json!({});
        assert_eq!(
            count_argument(&args, &["line_limit"], 1, Some(1000)),
            Ok(Some(1000))
        );
        assert_eq!(boolean_argument(&args, &["replace_all"], false), Ok(false));
    }

    #[test]
    fn a_null_parameter_is_absent_but_zero_is_a_value() {
        let args = json!({"line_limit": null});
        assert_eq!(
            count_argument(&args, &["line_limit"], 1, Some(1000)),
            Ok(Some(1000))
        );
        // 0 is a value, so it is judged against the domain rather than treated as "unset".
        let zero = json!({"line_limit": 0});
        let refusal = count_argument(&zero, &["line_limit"], 1, Some(1000)).unwrap_err();
        assert!(refusal.contains("line_limit"), "{refusal}");
        assert!(refusal.contains(">= 1"), "{refusal}");
    }

    #[test]
    fn a_negative_count_is_refused_rather_than_read_as_a_slice_from_the_end() {
        let args = json!({"line_limit": -3});
        let refusal = count_argument(&args, &["line_limit"], 1, Some(1000)).unwrap_err();
        assert!(refusal.contains("-3"), "the value is named: {refusal}");
        assert!(
            refusal.contains("refused rather than answered with a different reading"),
            "{refusal}"
        );
    }

    #[test]
    fn a_boolean_is_not_a_count() {
        let args = json!({"line_limit": true});
        let refusal = count_argument(&args, &["line_limit"], 1, None).unwrap_err();
        assert!(refusal.contains("a boolean is not a count"), "{refusal}");
    }

    #[test]
    fn a_fractional_count_is_refused_and_an_integral_float_is_read() {
        let fractional = json!({"line_limit": 2.5});
        assert!(count_argument(&fractional, &["line_limit"], 1, None).is_err());
        let integral = json!({"line_limit": 3.0});
        assert_eq!(
            count_argument(&integral, &["line_limit"], 1, None),
            Ok(Some(3))
        );
    }

    #[test]
    fn the_refusal_names_the_spelling_the_caller_used() {
        let args = json!({"limit": 0});
        let refusal = count_argument(&args, &["line_limit", "limit"], 1, None).unwrap_err();
        assert!(refusal.starts_with("limit "), "{refusal}");
    }

    #[test]
    fn the_two_string_spellings_of_a_boolean_are_read_as_the_value() {
        assert_eq!(
            boolean_argument(&json!({"replace_all": "true"}), &["replace_all"], false),
            Ok(true)
        );
        assert_eq!(
            boolean_argument(&json!({"replace_all": " FALSE "}), &["replace_all"], true),
            Ok(false)
        );
        assert_eq!(
            boolean_argument(&json!({"replace_all": true}), &["replace_all"], false),
            Ok(true)
        );
    }

    #[test]
    fn any_other_value_for_a_boolean_is_refused_rather_than_coerced() {
        // The measured bug was `"false"` read as truthy. The fix is not "read strings better";
        // it is that a value which is not a boolean has no reading at all.
        for value in [json!("yes"), json!("no"), json!(0), json!(1), json!("")] {
            let args = json!({"replace_all": value});
            let refusal = boolean_argument(&args, &["replace_all"], false).unwrap_err();
            assert!(
                refusal.contains("refused rather than run with a value read as its opposite"),
                "{refusal}"
            );
        }
    }

    #[test]
    fn a_missing_required_string_names_the_parameter() {
        let refusal = string_argument(&json!({}), &["file_path"]).unwrap_err();
        assert!(refusal.contains("file_path"), "{refusal}");
        assert!(refusal.contains("required"), "{refusal}");
    }

    #[test]
    fn a_non_string_is_refused_rather_than_stringified() {
        assert!(string_argument(&json!({"file_path": 42}), &["file_path"]).is_err());
        assert_eq!(optional_string_argument(&json!({}), &["intent"]), Ok(None));
    }
}
