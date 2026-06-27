'use strict';

/**
 * Standard Claude tier model names ccbud advertises to clients.
 *
 * The gateway accepts any claude-* name and tier-maps it onto the active provider (see resolveRouting):
 * opus/sonnet → the provider's main model, haiku → its small/fast model. Claude Desktop's "Gateway"
 * mode needs an explicit model list (`inferenceModels`) whose names (a) pass its client-side validation
 * that rejects names without an Anthropic keyword, and (b) match what the gateway returns from
 * /v1/models. Exposing these three names in BOTH places lets a freshly-installed Claude Desktop pick a
 * model and drive the gateway with zero per-user setup — the actual upstream is the user's provider.
 *
 * Version numbers are cosmetic here: ccbud never forwards these names to Anthropic; it routes by tier.
 */
const CLAUDE_TIER_MODELS = [
  { name: 'claude-opus-4-6', tier: 'opus' },
  { name: 'claude-sonnet-4-6', tier: 'sonnet', familyDefault: true },
  { name: 'claude-haiku-4-5', tier: 'haiku' },
];

module.exports = { CLAUDE_TIER_MODELS };
