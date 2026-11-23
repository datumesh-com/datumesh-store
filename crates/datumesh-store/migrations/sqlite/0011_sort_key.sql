-- datumesh-store SQLite schema, migration 11 (additions only; PostgreSQL's migration 15): the
-- sort key of each cell (CONTRACT-0.5.1 S1) and the class bindings' index by class hash (S3).
--
-- cells.sort_key is the cell's arguments in ascending name order, each part from the SQLite
-- function datumesh_sort_item(type, value) (registered on every connection; the Rust twin is
-- slices::sort_item, and the two must agree). The cells are inserted before their arguments,
-- so the trigger on cell_args refreshes the cell's key after each argument is inserted: the
-- last one leaves the whole key. cells is append-only, so its update trigger refuses updates
-- of the identity columns only; the key is derived and may be filled once a row is there.
-- The rows already stored get their key from migrate::sqlite's step after this file (it
-- calls the function, which a bare connection of the tests does not have).

DROP TRIGGER cells_no_update;
CREATE TRIGGER cells_no_update BEFORE UPDATE OF cell_id, fn_hash, ns, fn, nargs ON cells
    BEGIN SELECT RAISE(ABORT, 'datumesh: UPDATE on cells refused: the table is append-only'); END;

ALTER TABLE cells ADD COLUMN sort_key BLOB NOT NULL DEFAULT x'';

CREATE TRIGGER cell_args_sort_key AFTER INSERT ON cell_args
    BEGIN
        UPDATE cells SET sort_key = coalesce(unhex((
                SELECT group_concat(hex(datumesh_sort_item(a.type, a.value)), '' ORDER BY a.name)
                  FROM cell_args a WHERE a.cell_id = NEW.cell_id)), x'')
         WHERE cell_id = NEW.cell_id;
    END;

CREATE INDEX cells_sort_key ON cells (fn_hash, ns, fn, sort_key);

CREATE INDEX class_bindings_by_hash ON class_bindings (class_hash, class_name, tx);
