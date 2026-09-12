from __future__ import annotations

from dataclasses import dataclass

from .models import Usage

PRICING_AS_OF = "2026-08-12"


@dataclass(frozen=True, slots=True)
class Rate:
    input_per_million: float
    output_per_million: float
    cached_input_per_million: float | None = None
    cache_write_per_million: float | None = None


# API-equivalent list prices, not subscription billing. Unknown/private model
# slugs intentionally have no fallback rate.
RATES: dict[str, Rate] = {
    "gpt-5.4": Rate(2.50, 15.00, 0.25),
    "gpt-5.4-mini": Rate(0.75, 4.50, 0.075),
    "gpt-5.4-nano": Rate(0.20, 1.25, 0.02),
    "gpt-5.5": Rate(12.50, 75.00, 1.25),
    "claude-opus-5": Rate(5.00, 25.00, 0.50, 6.25),
    "claude-opus-4-6": Rate(5.00, 25.00, 0.50, 6.25),
    "claude-opus-4-7": Rate(5.00, 25.00, 0.50, 6.25),
    "claude-opus-4-8": Rate(5.00, 25.00, 0.50, 6.25),
    "claude-sonnet-5": Rate(2.00, 10.00, 0.20, 2.50),
    "claude-haiku-4-5": Rate(1.00, 5.00, 0.10, 1.25),
}


def find_rate(model: str | None) -> Rate | None:
    if not model:
        return None
    normalized = model.lower()
    if normalized in RATES:
        return RATES[normalized]
    for key in sorted(RATES, key=len, reverse=True):
        if normalized.startswith(key + "-"):
            return RATES[key]
    return None


def estimate_cost(usage: Usage, *, cached_in_input: bool = True) -> float | None:
    rate = find_rate(usage.model)
    if rate is None:
        return None
    cached = (
        min(usage.cached_input_tokens, usage.input_tokens)
        if cached_in_input
        else usage.cached_input_tokens
    )
    uncached = (
        max(0, usage.input_tokens - cached) if cached_in_input else usage.input_tokens
    )
    cached_rate = rate.cached_input_per_million or rate.input_per_million
    write_rate = rate.cache_write_per_million or rate.input_per_million
    return (
        uncached * rate.input_per_million
        + cached * cached_rate
        + usage.output_tokens * rate.output_per_million
        + usage.cache_write_tokens * write_rate
    ) / 1_000_000
