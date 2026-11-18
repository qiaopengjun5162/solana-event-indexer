-- 幂等入库：同一笔交易在同一个池子里的同一方向只保留一条。
-- 不加约束的话，重复 backfill 会把同一批 swap 再插一遍，价差统计被放大。

CREATE UNIQUE INDEX IF NOT EXISTS ux_swaps_sig_pool_direction
    ON swaps (signature, pool_id, token_in);
