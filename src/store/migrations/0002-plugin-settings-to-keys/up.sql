-- Plugins' settings were sections under `core.pluginSettings` (scope
-- `plugin`, id the plugin's): a JSON object of the fields the user set.
-- Each is now a key its plugin owns. The only field there was is the
-- clipboard's `syncEnabled`, which becomes `clipboard.syncEnabled`. For a
-- boolean, `json_type` is its JSON text; the CASE keeps a row that isn't
-- JSON from failing the migration.
INSERT OR REPLACE INTO configs (key, scope, id, value, updated_at)
SELECT 'clipboard.syncEnabled', '', '', json_type(value, '$.syncEnabled'), updated_at
FROM configs
WHERE key = 'core.pluginSettings'
  AND scope = 'plugin'
  AND id = 'clipboard'
  AND CASE WHEN json_valid(value) THEN json_type(value, '$.syncEnabled') END
      IN ('true', 'false');

DELETE FROM configs WHERE scope = 'plugin';
