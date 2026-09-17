-- Configurable Claude summary output budget, pinned to LF in .gitattributes.
-- NULL uses the application's model-aware default rather than hardcoded 2048.
ALTER TABLE settings ADD COLUMN summaryMaxTokens INTEGER;
