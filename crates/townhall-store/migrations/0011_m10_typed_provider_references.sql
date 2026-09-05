-- ADR-030: effect provider references used to be untyped bare strings.
-- Existing rows can only be council references; tag them before readers begin
-- requiring an explicit kind. New writes use council:<ref> or payment:<ref>.
UPDATE effect_intents
SET provider_reference = 'council:' || provider_reference
WHERE provider_reference IS NOT NULL
  AND provider_reference NOT LIKE 'council:%'
  AND provider_reference NOT LIKE 'payment:%';
