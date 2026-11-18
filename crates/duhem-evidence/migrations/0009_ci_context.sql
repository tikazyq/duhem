-- Optional CI provenance; historical runs remain unattributed.
ALTER TABLE runs ADD COLUMN branch TEXT;
ALTER TABLE runs ADD COLUMN pr TEXT;
ALTER TABLE runs ADD COLUMN ci_url TEXT;
