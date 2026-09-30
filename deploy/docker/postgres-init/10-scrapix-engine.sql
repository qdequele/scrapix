-- The engine's own database (Lab split): the Rails app keeps `scrapix`, the
-- Rust engine stores its job history, lab events outbox and accounting in
-- `scrapix_engine`. Runs only when the postgres data volume is first created;
-- an older dev volume needs `createdb -h localhost -p 5433 -U scrapix scrapix_engine`.
CREATE DATABASE scrapix_engine;
