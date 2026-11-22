-- datumesh-store schema, migration 14 (0.6.0, CONTRACT-round-0.7 R1 and R2): withdrawals and
-- closed names. Replacements and relaxations only; no existing row changes.
--
-- Withdrawals (R1): a withdrawal receipt (8.1) has no type and no value, so its receipts row
-- has value_type = value = '' (the empty text, which no valued receipt has: a type is a name).
-- The two functions of migration 4 give that for a receipt without a value; a valued receipt's
-- columns are the same as before. A version whose receipt has value_type = '' is withdrawn.
CREATE OR REPLACE FUNCTION datumesh_receipt_value_type(bytes bytea) RETURNS text
    LANGUAGE sql IMMUTABLE STRICT PARALLEL SAFE
    AS $$ SELECT COALESCE((convert_from(bytes, 'UTF8')::json) ->> 'type', '') $$;

CREATE OR REPLACE FUNCTION datumesh_receipt_value(bytes bytea) RETURNS text
    LANGUAGE sql IMMUTABLE STRICT PARALLEL SAFE
    AS $$ SELECT COALESCE((convert_from(bytes, 'UTF8')::json) ->> 'value', '') $$;

-- Closed names (R2): a binding row whose function hash (class hash) is NULL closes its name
-- from its tx on: the name means nothing from then on, until it is bound again (a later tx).
-- The append-only triggers, the order and the one-namespace checks of the two tables are
-- untouched; a closed row is an ordinary row of the binding history (with written_at).
ALTER TABLE name_bindings ALTER COLUMN fn_hash DROP NOT NULL;
ALTER TABLE class_bindings ALTER COLUMN class_hash DROP NOT NULL;
