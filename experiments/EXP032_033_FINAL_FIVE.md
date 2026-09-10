# EXP032/033 — final score-gated five-family portfolio

## Question

Do the later H3/H5 structures, causal family filters and prior-month score gate
transfer together on the independent source and then survive executable
quotes, costs and one shared account?

The five books are H3_F4 outward-1 CE short plus H5_F1/F2/F3/F5 bearish PE
verticals. Their exact widths and filters are in `docs/EXPERIMENTS.md`.

## Run

Follow sections 1–5 of `docs/RUNBOOK.md`, then run the exact command in section
5. The launcher binds the Git revision, calendar, source contract, policy
configuration and reference model SHA. The Rust policy recomputes the score,
checks the event-time chain, official lot, expiry, strike, diagnostic and
filter evidence, then emits one atomic basket intent.

For the funded lane, connect those intents to the generic engine as one fixed
₹10 lakh, non-compounding account. This repository does not vendor that engine
or claim a funded result by itself.
