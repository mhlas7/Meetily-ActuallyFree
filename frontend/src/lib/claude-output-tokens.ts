import limits from './claude-output-limits.json';

// Shared with Rust. Conservative fallback for unknown models; these are app
// budgets, not a promise to use every token a model might support.
export function claudeOutputBudget(model: string) {
  const name = model.toLowerCase();
  const maximum = Math.min(limits.applicationLimit,
    limits.models.find(({ prefix }) => name === prefix || name.startsWith(prefix.endsWith('-') ? prefix : `${prefix}-`))?.limit ?? limits.fallbackLimit);
  return { maximum, defaultTokens: Math.min(limits.defaultTokens, maximum) };
}

export function parseClaudeOutputTokens(value: string, model: string): number | null {
  const text = value.trim();
  if (!text) return null;
  const { maximum } = claudeOutputBudget(model);
  if (!/^\d+$/.test(text) || !Number.isSafeInteger(Number(text)) || Number(text) < 1 || Number(text) > maximum) {
    throw new Error(`Maximum summary length must be a whole number between 1 and ${maximum.toLocaleString()}, or left empty.`);
  }
  return Number(text);
}
