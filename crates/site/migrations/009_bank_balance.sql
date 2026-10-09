ALTER TABLE players ADD COLUMN bank_balance BIGINT NOT NULL DEFAULT 0 CHECK (bank_balance >= 0);
