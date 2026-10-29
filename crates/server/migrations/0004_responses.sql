-- Responses API (M4). Per model: whether a translated answer turns the
-- upstream's reasoning text (llama.cpp `reasoning_content`, OpenRouter
-- `reasoning`) into Responses `reasoning` items, or leaves it out.
ALTER TABLE models ADD COLUMN reasoning_items boolean NOT NULL DEFAULT true;
