ALTER TABLE lp_risk_events
    ADD COLUMN condition_id TEXT;

ALTER TABLE lp_heartbeats
    ADD COLUMN condition_id TEXT;

ALTER TABLE lp_reports
    ADD COLUMN condition_id TEXT;

ALTER TABLE lp_control_actions
    ADD COLUMN condition_id TEXT;

CREATE INDEX IF NOT EXISTS idx_lp_risk_events_condition_id ON lp_risk_events(condition_id);
CREATE INDEX IF NOT EXISTS idx_lp_heartbeats_condition_id ON lp_heartbeats(condition_id);
CREATE INDEX IF NOT EXISTS idx_lp_reports_condition_id ON lp_reports(condition_id);
CREATE INDEX IF NOT EXISTS idx_lp_control_actions_condition_id ON lp_control_actions(condition_id);
