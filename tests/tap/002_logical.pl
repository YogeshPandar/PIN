# logical changes build subscriber-local physical identities, never ship postings.
# contracts: docs/api-evidence.md, g8-operations.
use strict;
use warnings FATAL => 'all';
use PostgreSQL::Test::Cluster;
use PostgreSQL::Test::Utils;
use Test::More;

my $config = q{
shared_preload_libraries = 'pin'
wal_level = logical
fsync = on
full_page_writes = on
synchronous_commit = on
max_replication_slots = 10
max_wal_senders = 10
max_logical_replication_workers = 4
max_worker_processes = 12
max_parallel_workers_per_gather = 0
autovacuum = off
statement_timeout = '30s'
};
my $pub = PostgreSQL::Test::Cluster->new('g8_publisher');
$pub->init(allows_streaming => 'logical', extra => ['--encoding=UTF8', '--locale=C']);
$pub->append_conf('postgresql.conf', $config);
$pub->start;
my $sub = PostgreSQL::Test::Cluster->new('g8_subscriber');
$sub->init(extra => ['--encoding=UTF8', '--locale=C']);
$sub->append_conf('postgresql.conf', $config);
$sub->start;
for my $node ($pub, $sub)
{
    $node->safe_psql('postgres', q{
CREATE EXTENSION pin;
CREATE TABLE docs (id integer PRIMARY KEY, body text);
CREATE INDEX docs_pin ON docs USING pin(body);
});
    is($node->safe_psql('postgres', 'SELECT pin.build_profile()'), 'normal', 'logical node uses normal library');
}
$pub->safe_psql('postgres', q{
INSERT INTO docs VALUES (1, 'alpha'), (2, 'beta');
CREATE PUBLICATION pin_pub FOR TABLE docs;
});
my $connection = $pub->connstr('postgres');
$connection =~ s/'/''/g;
$sub->safe_psql('postgres', "CREATE SUBSCRIPTION pin_sub CONNECTION '$connection' PUBLICATION pin_pub");
$sub->poll_query_until('postgres', q{
SELECT count(*) = 1 AND bool_and(srsubstate = 'r') FROM pg_subscription_rel
}) or BAIL_OUT('subscriber initial synchronization did not complete');

sub check_rows
{
    my ($expected, $label) = @_;
    my $rows = "SELECT coalesce(string_agg(id::text || ':' || body, '|' ORDER BY id), '') FROM docs";
    $sub->poll_query_until('postgres', "SELECT ($rows) = '$expected'")
        or BAIL_OUT("logical apply timed out: $label");
    for my $term ('alpha', 'beta', 'NOT alpha')
    {
        my $query = "SELECT coalesce(array_agg(id ORDER BY id)::text, '{}') FROM docs "
            . "WHERE body OPERATOR(pin.@@@) pin.parse_query('$term')";
        my $off = 'SET enable_indexscan=off; SET enable_indexonlyscan=off; SET enable_bitmapscan=off;';
        my $on = 'SET enable_seqscan=off; SET enable_indexscan=off; SET enable_indexonlyscan=off; SET enable_bitmapscan=on;';
        my $plan = $sub->safe_psql('postgres', $on . 'EXPLAIN (FORMAT JSON) ' . $query);
        like($plan, qr/"Index Name": "docs_pin"/, "$label: subscriber uses local Pin index");
        is($sub->safe_psql('postgres', $on . $query), $sub->safe_psql('postgres', $off . $query),
            "$label: exact local identities for $term");
    }
}

check_rows('1:alpha|2:beta', 'initial copy');
$pub->safe_psql('postgres', q{
BEGIN;
UPDATE docs SET body = 'beta' WHERE id = 1;
DELETE FROM docs WHERE id = 2;
INSERT INTO docs VALUES (3, 'alpha');
COMMIT;
});
check_rows('1:beta|3:alpha', 'replica-identity update and delete');
$sub->safe_psql('postgres', 'VACUUM docs');
check_rows('1:beta|3:alpha', 'subscriber vacuum');
$pub->safe_psql('postgres', 'TRUNCATE docs');
check_rows('', 'replicated truncate');
$pub->safe_psql('postgres', "INSERT INTO docs VALUES (1, 'beta')");
check_rows('1:beta', 'reused subscriber identity');
$sub->safe_psql('postgres', 'DROP SUBSCRIPTION pin_sub');
is($pub->safe_psql('postgres', "SELECT count(*) FROM pg_replication_slots WHERE slot_name = 'pin_sub'"),
    '0', 'subscription cleanup removes its publisher slot');
$sub->stop;
$pub->stop;
done_testing();
