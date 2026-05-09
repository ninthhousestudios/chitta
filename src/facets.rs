use std::collections::BTreeSet;
use std::fmt::Write;

use crate::tools::search::AppliesTo;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Facets {
    pub domains: Vec<String>,
    pub skills: Vec<String>,
    pub projects: Vec<String>,
    pub situations: Vec<String>,
}

impl Facets {
    pub fn is_empty(&self) -> bool {
        self.domains.is_empty()
            && self.skills.is_empty()
            && self.projects.is_empty()
            && self.situations.is_empty()
    }
}

impl From<AppliesTo> for Facets {
    fn from(a: AppliesTo) -> Self {
        Self {
            domains: a.domains.unwrap_or_default(),
            skills: a.skills.unwrap_or_default(),
            projects: a.projects.unwrap_or_default(),
            situations: a.situations.unwrap_or_default(),
        }
    }
}

impl Facets {
    /// Generates `AND applies_to_X @> $N` clauses for each non-empty facet.
    /// Returns the SQL fragment and the values to bind, in parameter order.
    pub fn sql_contains_filters(&self, start_param: i32) -> (String, Vec<&[String]>) {
        const COLUMNS: &[&str] = &[
            "applies_to_domains",
            "applies_to_skills",
            "applies_to_projects",
            "applies_to_situations",
        ];
        let vecs = [
            &self.domains,
            &self.skills,
            &self.projects,
            &self.situations,
        ];
        let mut sql = String::new();
        let mut binds: Vec<&[String]> = Vec::new();
        let mut idx = start_param;
        for (col, v) in COLUMNS.iter().zip(vecs.iter()) {
            if !v.is_empty() {
                write!(sql, " AND {col} @> ${idx}").unwrap();
                binds.push(v.as_slice());
                idx += 1;
            }
        }
        (sql, binds)
    }
}

pub trait HasFacets {
    fn domains(&self) -> &[String];
    fn skills(&self) -> &[String];
    fn projects(&self) -> &[String];
    fn situations(&self) -> &[String];
}

impl HasFacets for Facets {
    fn domains(&self) -> &[String] {
        &self.domains
    }
    fn skills(&self) -> &[String] {
        &self.skills
    }
    fn projects(&self) -> &[String] {
        &self.projects
    }
    fn situations(&self) -> &[String] {
        &self.situations
    }
}

impl Facets {
    pub fn distinct_union<T: HasFacets>(items: &[T]) -> Facets {
        let mut domains = BTreeSet::new();
        let mut skills = BTreeSet::new();
        let mut projects = BTreeSet::new();
        let mut situations = BTreeSet::new();
        for item in items {
            domains.extend(item.domains().iter().cloned());
            skills.extend(item.skills().iter().cloned());
            projects.extend(item.projects().iter().cloned());
            situations.extend(item.situations().iter().cloned());
        }
        Facets {
            domains: domains.into_iter().collect(),
            skills: skills.into_iter().collect(),
            projects: projects.into_iter().collect(),
            situations: situations.into_iter().collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_empty() {
        let f = Facets::default();
        assert!(f.is_empty());
        assert!(f.domains.is_empty());
        assert!(f.skills.is_empty());
        assert!(f.projects.is_empty());
        assert!(f.situations.is_empty());
    }

    #[test]
    fn is_empty_false_when_any_facet_populated() {
        let cases = vec![
            Facets {
                domains: vec!["rust".into()],
                ..Default::default()
            },
            Facets {
                skills: vec!["review".into()],
                ..Default::default()
            },
            Facets {
                projects: vec!["chitta".into()],
                ..Default::default()
            },
            Facets {
                situations: vec!["debugging".into()],
                ..Default::default()
            },
        ];
        for f in &cases {
            assert!(!f.is_empty(), "expected non-empty for {f:?}");
        }
    }

    #[test]
    fn from_applies_to_all_none() {
        let a = AppliesTo::default();
        let f = Facets::from(a);
        assert!(f.is_empty());
    }

    #[test]
    fn from_applies_to_all_some() {
        let a = AppliesTo {
            domains: Some(vec!["rust".into()]),
            skills: Some(vec!["review".into(), "reflect".into()]),
            projects: Some(vec!["chitta".into()]),
            situations: Some(vec!["debugging".into()]),
        };
        let f = Facets::from(a);
        assert_eq!(f.domains, vec!["rust"]);
        assert_eq!(f.skills, vec!["review", "reflect"]);
        assert_eq!(f.projects, vec!["chitta"]);
        assert_eq!(f.situations, vec!["debugging"]);
    }

    #[test]
    fn sql_contains_filters_zero_facets() {
        let f = Facets::default();
        let (sql, binds) = f.sql_contains_filters(1);
        assert_eq!(sql, "");
        assert!(binds.is_empty());
    }

    #[test]
    fn sql_contains_filters_one_facet() {
        let f = Facets {
            domains: vec!["rust".into()],
            ..Default::default()
        };
        let (sql, binds) = f.sql_contains_filters(5);
        assert_eq!(sql, " AND applies_to_domains @> $5");
        assert_eq!(binds.len(), 1);
        assert_eq!(binds[0], &["rust".to_string()]);
    }

    #[test]
    fn sql_contains_filters_two_facets() {
        let f = Facets {
            skills: vec!["review".into()],
            situations: vec!["debugging".into()],
            ..Default::default()
        };
        let (sql, binds) = f.sql_contains_filters(3);
        assert_eq!(
            sql,
            " AND applies_to_skills @> $3 AND applies_to_situations @> $4"
        );
        assert_eq!(binds.len(), 2);
    }

    #[test]
    fn sql_contains_filters_all_four_facets() {
        let f = Facets {
            domains: vec!["rust".into()],
            skills: vec!["review".into()],
            projects: vec!["chitta".into()],
            situations: vec!["debugging".into()],
        };
        let (sql, binds) = f.sql_contains_filters(7);
        assert!(sql.contains("applies_to_domains @> $7"));
        assert!(sql.contains("applies_to_skills @> $8"));
        assert!(sql.contains("applies_to_projects @> $9"));
        assert!(sql.contains("applies_to_situations @> $10"));
        assert_eq!(binds.len(), 4);
    }

    #[test]
    fn distinct_union_empty() {
        let items: Vec<Facets> = vec![];
        let result = Facets::distinct_union(&items);
        assert!(result.is_empty());
    }

    #[test]
    fn distinct_union_deduplicates_and_sorts() {
        let items = vec![
            Facets {
                domains: vec!["rust".into(), "python".into()],
                skills: vec!["review".into()],
                ..Default::default()
            },
            Facets {
                domains: vec!["rust".into(), "go".into()],
                skills: vec!["reflect".into(), "review".into()],
                projects: vec!["chitta".into()],
                ..Default::default()
            },
        ];
        let result = Facets::distinct_union(&items);
        assert_eq!(result.domains, vec!["go", "python", "rust"]);
        assert_eq!(result.skills, vec!["reflect", "review"]);
        assert_eq!(result.projects, vec!["chitta"]);
        assert!(result.situations.is_empty());
    }

    #[test]
    fn distinct_union_with_has_facets_trait() {
        struct Row {
            d: Vec<String>,
            s: Vec<String>,
        }
        impl HasFacets for Row {
            fn domains(&self) -> &[String] {
                &self.d
            }
            fn skills(&self) -> &[String] {
                &self.s
            }
            fn projects(&self) -> &[String] {
                &[]
            }
            fn situations(&self) -> &[String] {
                &[]
            }
        }
        let rows = vec![
            Row {
                d: vec!["a".into()],
                s: vec!["x".into()],
            },
            Row {
                d: vec!["b".into(), "a".into()],
                s: vec!["y".into()],
            },
        ];
        let result = Facets::distinct_union(&rows);
        assert_eq!(result.domains, vec!["a", "b"]);
        assert_eq!(result.skills, vec!["x", "y"]);
    }
}
