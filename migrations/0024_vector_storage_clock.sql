-- Published platform allocated-index capacity, not estimated embedding tokens.
CREATE TABLE file_search_storage_clocks (
  id TEXT PRIMARY KEY,
  account_id TEXT NOT NULL REFERENCES accounts(id),
  vector_store_id TEXT NOT NULL,
  file_id TEXT NOT NULL,
  generation TEXT NOT NULL,
  dimensions INTEGER NOT NULL CHECK(dimensions>0),
  pricing_json TEXT NOT NULL CHECK(json_valid(pricing_json)),
  created_at_ms INTEGER NOT NULL,
  billed_through_ms INTEGER NOT NULL,
  carry_dimension_ms TEXT NOT NULL DEFAULT '0',
  removed_at_ms INTEGER,
  finalized INTEGER NOT NULL DEFAULT 0 CHECK(finalized IN (0,1)),
  pending_json TEXT CHECK(pending_json IS NULL OR json_valid(pending_json)),
  debt INTEGER NOT NULL DEFAULT 0 CHECK(debt IN (0,1)),
  next_due_ms INTEGER NOT NULL,
  lease_id TEXT,
  lease_until INTEGER NOT NULL DEFAULT 0,
  UNIQUE(account_id,vector_store_id,file_id,generation),
  CHECK(billed_through_ms>=created_at_ms),
  CHECK(removed_at_ms IS NULL OR removed_at_ms>=created_at_ms)
);
CREATE INDEX file_search_storage_due ON file_search_storage_clocks(finalized,next_due_ms,lease_until);
CREATE INDEX file_search_storage_owner ON file_search_storage_clocks(account_id,vector_store_id,debt);
