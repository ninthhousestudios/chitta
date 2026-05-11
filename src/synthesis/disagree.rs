use uuid::Uuid;

use crate::db::MemoryRow;

pub fn find_disagree_targets(rows: &[MemoryRow]) -> Vec<Uuid> {
    rows.iter()
        .filter(|r| {
            r.tags.iter().any(|t| t == "feedback") && r.tags.iter().any(|t| t == "disagree")
        })
        .filter_map(|r| {
            r.external_refs
                .as_ref()?
                .as_array()?
                .iter()
                .find_map(|ref_obj| {
                    if ref_obj.get("kind")?.as_str()? == "memory" {
                        Uuid::parse_str(ref_obj.get("ref")?.as_str()?).ok()
                    } else {
                        None
                    }
                })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::synthesis::test_support::make_row;
    use uuid::Uuid;

    #[test]
    fn disagree_targets_extracted() {
        let target_id = Uuid::now_v7();
        let mut row = make_row(
            Uuid::now_v7(),
            "Feedback: disagree with memory",
            "observation",
        );
        row.tags = vec!["feedback".into(), "disagree".into()];
        row.external_refs = Some(serde_json::json!([
            {"kind": "memory", "ref": target_id.to_string()}
        ]));

        let targets = find_disagree_targets(&[row]);
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0], target_id);
    }

    #[test]
    fn non_disagree_rows_ignored() {
        let mut row = make_row(Uuid::now_v7(), "some observation", "observation");
        row.tags = vec!["feedback".into(), "agree".into()];
        row.external_refs = Some(serde_json::json!([
            {"kind": "memory", "ref": Uuid::now_v7().to_string()}
        ]));

        let targets = find_disagree_targets(&[row]);
        assert!(targets.is_empty());
    }

    #[test]
    fn disagree_without_refs_skipped() {
        let mut row = make_row(Uuid::now_v7(), "disagree but no refs", "observation");
        row.tags = vec!["feedback".into(), "disagree".into()];

        let targets = find_disagree_targets(&[row]);
        assert!(targets.is_empty());
    }
}
