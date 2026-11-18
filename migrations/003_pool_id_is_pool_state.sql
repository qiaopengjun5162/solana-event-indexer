-- pool_id 语义变更（2026-11-18）
--
-- 旧版本的 parser 把 swap 指令里「金库账户的 owner」当成了 pool_id。
-- 那个地址是 CPMM 程序派生出来的 authority PDA（有时甚至是别的 DEX 的池子），
-- 链上既没有数据、也不可能 getAccountInfo 读出两个金库和 mint，
-- 于是「按池子读储备、算真实中间价」这条路根本走不通。
--
-- 实测：库里 130 个 pool_id 里，排前面的全是 Orca Whirlpool、Meteora DLMM、
-- Raydium CLMM 的账户，只有极少数是 CPMM 自己的池子。
--
-- 旧行无法就地修正——要把每笔交易重新取一遍才能拿到 PoolState 地址，
-- 因此这里直接清空重灌。本地样本只有几百行，重灌一次几分钟。
TRUNCATE TABLE swaps RESTART IDENTITY;

COMMENT ON COLUMN swaps.pool_id IS
    'Raydium CPMM PoolState 账户地址（不是金库 owner PDA），可直接 getAccountInfo 读储备';
