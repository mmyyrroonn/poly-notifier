use poly_lp::rewards_report::RewardsReport;

#[test]
fn rewards_report_round_trips_minimal_payload() {
    let payload = r#"
    {
      "generated_at": "2026-03-25T07:35:14.562876+00:00",
      "scan_count": 2,
      "analyzed_count": 1,
      "skipped_count": 1,
      "distribution": {
        "min": "1",
        "median": "2",
        "p90": "3",
        "max": "4"
      },
      "analyses": [],
      "skipped": [
        {
          "condition_id": "condition-1",
          "slug": "slug-1",
          "question": "Question?",
          "reason": "unsupported_outcome_layout"
        }
      ]
    }
    "#;

    let report: RewardsReport = serde_json::from_str(payload).expect("deserialize report");

    assert_eq!(report.generated_at, "2026-03-25T07:35:14.562876+00:00");
    assert_eq!(report.scan_count, 2);
    assert_eq!(report.skipped[0].reason, "unsupported_outcome_layout");

    let rendered = serde_json::to_string(&report).expect("serialize report");
    assert!(rendered.contains("\"scan_count\":2"));
    assert!(rendered.contains("\"condition_id\":\"condition-1\""));
}
