//! Bounded, toolkit/IO-free graph scope values shared by workspace and core.
use serde::{Deserialize, Serialize};

/// A predicate over current structural metadata and saved search membership.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum NotePredicate {
    /// Every note.
    #[default]
    All,
    /// Exact normalized tag (without `#`).
    Tag { name: String },
    /// Exact native path prefix, ending on a component boundary.
    PathPrefix { native: Vec<u8> },
    /// A typed scalar property value.
    Property { key: String, value: PropertyValue },
    /// Existing user query syntax, evaluated against saved content.
    SavedQuery { query: String },
    /// All children must match.
    And { clauses: Vec<NotePredicate> },
    /// Any child may match.
    Or { clauses: Vec<NotePredicate> },
    /// Complement within eligible notes.
    Not { clause: Box<NotePredicate> },
}

/// Scalar property representation; numbers retain their spelling, never NaN.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum PropertyValue {
    Text(String),
    Number(String),
    Bool(bool),
    Date(String),
    DateTime(String),
    Null,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    In,
    Out,
    #[default]
    Both,
}

/// Durable neighborhood root: exact native path, never a cache entity ID.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Neighborhood {
    pub root_native: Vec<u8>,
    pub depth: u8,
    pub direction: Direction,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GraphScope {
    #[serde(default)]
    pub predicate: NotePredicate,
    #[serde(default)]
    pub neighborhood: Option<Neighborhood>,
    #[serde(default)]
    pub include_attachments: bool,
    #[serde(default)]
    pub include_embeds: bool,
    #[serde(default)]
    pub show_unresolved: bool,
}

impl GraphScope {
    /// Validate before query admission and after bounded workspace decode.
    /// At most 64 predicate records, depth 16, 8 KiB per string/path.
    pub fn validate(&self) -> Result<(), &'static str> {
        let mut work = vec![(&self.predicate, 1)];
        let mut count = 0;
        while let Some((predicate, depth)) = work.pop() {
            count += 1;
            if count > 64 || depth > 16 {
                return Err("filter exceeds 64 clauses or depth 16");
            }
            let text = match predicate {
                NotePredicate::Tag { name } => Some(name.as_str()),
                NotePredicate::Property { key, value } => {
                    match value {
                        PropertyValue::Number(number) => {
                            if number.len() > 8192
                                || !number.parse::<f64>().is_ok_and(f64::is_finite)
                            {
                                return Err("property number must be finite");
                            }
                        }
                        PropertyValue::Text(s)
                        | PropertyValue::Date(s)
                        | PropertyValue::DateTime(s)
                            if s.len() > 8192 =>
                        {
                            return Err("property value exceeds 8 KiB");
                        }
                        _ => {}
                    }
                    Some(key.as_str())
                }
                NotePredicate::SavedQuery { query } => Some(query.as_str()),
                NotePredicate::PathPrefix { native } => {
                    if native.len() > 8192 || native.contains(&0) {
                        return Err("invalid native path prefix");
                    }
                    None
                }
                NotePredicate::And { clauses } | NotePredicate::Or { clauses } => {
                    if clauses.is_empty() || clauses.len() > 64 {
                        return Err("filter group needs 1–64 clauses");
                    }
                    work.extend(clauses.iter().map(|clause| (clause, depth + 1)));
                    None
                }
                NotePredicate::Not { clause } => {
                    work.push((clause, depth + 1));
                    None
                }
                NotePredicate::All => None,
            };
            if text.is_some_and(|s| s.len() > 8192 || s.contains('\0')) {
                return Err("filter text exceeds 8 KiB or contains NUL");
            }
        }
        if self.neighborhood.as_ref().is_some_and(|n| {
            n.depth > 6
                || n.root_native.is_empty()
                || n.root_native.len() > 8192
                || n.root_native.contains(&0)
        }) {
            return Err("invalid neighborhood root or depth (0–6)");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope(predicate: NotePredicate) -> GraphScope {
        GraphScope {
            predicate,
            ..GraphScope::default()
        }
    }

    #[test]
    fn every_scalar_and_boolean_group_validates_and_round_trips() {
        let mut clauses = vec![
            NotePredicate::All,
            NotePredicate::Tag {
                name: "idea".into(),
            },
            NotePredicate::PathPrefix {
                native: b"Systems".to_vec(),
            },
            NotePredicate::SavedQuery {
                query: "tag:idea AND network".into(),
            },
        ];
        for value in [
            PropertyValue::Text("a".into()),
            PropertyValue::Number("2.5".into()),
            PropertyValue::Bool(true),
            PropertyValue::Date("2026-10-30".into()),
            PropertyValue::DateTime("2026-10-30T00:00:00Z".into()),
            PropertyValue::Null,
        ] {
            clauses.push(NotePredicate::Property {
                key: "field".into(),
                value,
            });
        }
        let value = scope(NotePredicate::And {
            clauses: vec![
                NotePredicate::Or { clauses },
                NotePredicate::Not {
                    clause: Box::new(NotePredicate::All),
                },
            ],
        });
        assert_eq!(value.validate(), Ok(()));
        let encoded = serde_json::to_vec(&value).unwrap();
        assert_eq!(
            serde_json::from_slice::<GraphScope>(&encoded).unwrap(),
            value
        );
        assert!(serde_json::from_str::<GraphScope>(r#"{"unexpected":true}"#).is_err());
    }

    #[test]
    fn malformed_bounds_are_rejected_before_admission() {
        for predicate in [
            NotePredicate::Tag {
                name: "a".repeat(8193),
            },
            NotePredicate::SavedQuery {
                query: "nul\0query".into(),
            },
            NotePredicate::PathPrefix { native: vec![0] },
            NotePredicate::PathPrefix {
                native: vec![1; 8193],
            },
            NotePredicate::And { clauses: vec![] },
            NotePredicate::Or {
                clauses: vec![NotePredicate::All; 65],
            },
            NotePredicate::And {
                clauses: vec![NotePredicate::All; 64],
            },
        ] {
            assert!(scope(predicate).validate().is_err());
        }
        assert!(
            scope(NotePredicate::And {
                clauses: vec![NotePredicate::All; 63]
            })
            .validate()
            .is_ok()
        );
        for value in [
            PropertyValue::Number("NaN".into()),
            PropertyValue::Number("1e999".into()),
            PropertyValue::Number("a".repeat(8193)),
            PropertyValue::Text("x".repeat(8193)),
            PropertyValue::Date("x".repeat(8193)),
            PropertyValue::DateTime("x".repeat(8193)),
        ] {
            assert!(
                scope(NotePredicate::Property {
                    key: "field".into(),
                    value
                })
                .validate()
                .is_err()
            );
        }
        let mut predicate = NotePredicate::All;
        for _ in 0..15 {
            predicate = NotePredicate::Not {
                clause: Box::new(predicate),
            };
        }
        assert!(scope(predicate.clone()).validate().is_ok());
        assert!(
            scope(NotePredicate::Not {
                clause: Box::new(predicate)
            })
            .validate()
            .is_err()
        );
        for (root, depth) in [
            (vec![], 2),
            (vec![1; 8193], 2),
            (vec![0], 2),
            (b"root.md".to_vec(), 7),
        ] {
            let value = GraphScope {
                neighborhood: Some(Neighborhood {
                    root_native: root,
                    depth,
                    direction: Direction::Both,
                }),
                ..GraphScope::default()
            };
            assert!(value.validate().is_err());
        }
        for direction in [Direction::In, Direction::Out, Direction::Both] {
            let value = GraphScope {
                neighborhood: Some(Neighborhood {
                    root_native: b"root.md".to_vec(),
                    depth: 6,
                    direction,
                }),
                ..GraphScope::default()
            };
            assert!(value.validate().is_ok());
        }
    }
}
