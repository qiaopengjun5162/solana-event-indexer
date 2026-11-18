CREATE TABLE IF NOT EXISTS swaps (
    id SERIAL PRIMARY KEY,
    signature TEXT NOT NULL,
    slot BIGINT NOT NULL,
    block_time TIMESTAMP,
    program_id TEXT NOT NULL,
    pool_id TEXT NOT NULL,
    token_in TEXT NOT NULL,
    token_out TEXT NOT NULL,
    amount_in_raw BIGINT NOT NULL,
    amount_out_raw BIGINT NOT NULL,
    amount_in NUMERIC NOT NULL,
    amount_out NUMERIC NOT NULL,
    priority_fee_lamports BIGINT NOT NULL DEFAULT 0,
    created_at TIMESTAMP DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_swaps_pool_slot ON swaps(pool_id, slot DESC);
CREATE INDEX IF NOT EXISTS idx_swaps_block_time ON swaps(block_time DESC);
CREATE INDEX IF NOT EXISTS idx_swaps_token_pair ON swaps(token_in, token_out);
