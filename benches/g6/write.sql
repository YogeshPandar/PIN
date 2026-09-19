\set id random(1, 20000)
-- change indexed text as well as the heap; every version keeps term membership.
UPDATE public.pin_g6_bench
SET updates = updates + 1,
    body = CASE WHEN updates % 2 = 0 THEN body || ' delta' ELSE left(body, length(body) - 6) END
WHERE id = :id;
