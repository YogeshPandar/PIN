# normal-build recovery and lifecycle qualification; no injection hooks.
# contracts: docs/api-evidence.md, g8-operations.
use strict;
use warnings FATAL => 'all';
use JSON::PP qw(decode_json);
use PostgreSQL::Test::Cluster;
use PostgreSQL::Test::Utils;
use Test::More;

my $revision = $ENV{PIN_BUILD_REVISION} // '';
$revision =~ /^[0-9a-f]{40}$/ or BAIL_OUT('set the candidate PIN_BUILD_REVISION');
my $settings = q{
shared_preload_libraries = 'pin'
fsync = on
full_page_writes = on
synchronous_commit = on
autovacuum = off
max_parallel_workers_per_gather = 0
statement_timeout = '30s'
lock_timeout = '10s'
};
my $bitmap = q{
SET enable_seqscan = off;
SET enable_indexscan = off;
SET enable_indexonlyscan = off;
SET enable_bitmapscan = on;
SET pin.enable_count_fastpath = off;
};
my $sequential = q{
SET enable_seqscan = on;
SET enable_indexscan = off;
SET enable_indexonlyscan = off;
SET enable_bitmapscan = off;
SET pin.enable_count_fastpath = off;
};

sub contains_index
{
    my ($node, $index) = @_;
    return 0 unless ref($node) eq 'HASH';
    return 1 if ($node->{'Node Type'} // '') eq 'Bitmap Index Scan'
        && ($node->{'Index Name'} // '') eq $index;
    for my $child (@{$node->{Plans} // []})
    {
        return 1 if contains_index($child, $index);
    }
    return 0;
}

sub query_sql
{
    my ($term) = @_;
    $term =~ s/'/''/g;
    return "SELECT coalesce(array_agg(id ORDER BY id)::text, '{}') FROM docs "
        . "WHERE body OPERATOR(pin.@@@) pin.parse_query('$term')";
}

sub result
{
    my ($node, $database, $term, $indexed) = @_;
    return $node->safe_psql($database,
        ($indexed ? $bitmap : $sequential) . query_sql($term));
}

sub equivalent
{
    my ($node, $database, $label, $reference) = @_;
    for my $term ('alpha', 'beta AND gamma', '"alpha beta"', 'NOT alpha', 'missing')
    {
        my $sql = query_sql($term);
        my $plan = decode_json($node->safe_psql($database,
            $bitmap . 'EXPLAIN (FORMAT JSON) ' . $sql));
        ok(contains_index($plan->[0]{Plan}, 'docs_pin'), "$label: actual Pin bitmap plan for $term");
        my $expected = result($node, $database, $term, 0);
        is(result($node, $database, $term, 1), $expected, "$label: exact identities for $term");
        is($expected, $reference->{$term}, "$label: preserved heap rows for $term") if defined $reference;
    }
}

sub snapshot_results
{
    my ($node) = @_;
    return {map { $_ => result($node, 'postgres', $_, 0) }
        ('alpha', 'beta AND gamma', '"alpha beta"', 'NOT alpha', 'missing')};
}

sub rejected
{
    my ($node, $sql, $pattern, $label) = @_;
    my ($status, $stdout, $stderr) = $node->psql('postgres', $sql,
        extra_params => ['--set=VERBOSITY=verbose']);
    isnt($status, 0, "$label: rejected");
    like($stderr, $pattern, "$label: diagnostic");
}

my $primary = PostgreSQL::Test::Cluster->new('g8_primary');
$primary->init(has_archiving => 1, allows_streaming => 1,
    extra => ['--encoding=UTF8', '--locale=C']);
$primary->append_conf('postgresql.conf', $settings);
$primary->start;
$primary->safe_psql('postgres', q{
CREATE EXTENSION pin;
CREATE TABLE docs (id integer PRIMARY KEY, body text, extra integer DEFAULT 0);
INSERT INTO docs SELECT i, CASE WHEN i % 3 = 0 THEN 'alpha beta' ELSE 'beta gamma' END, 0
FROM generate_series(1, 300) i;
CREATE INDEX docs_pin ON docs USING pin(body);
ANALYZE docs;
});
is($primary->safe_psql('postgres', 'SELECT pin.build_profile()'), 'normal', 'normal library loaded');
is($primary->safe_psql('postgres', 'SELECT pin.build_revision()'), $revision, 'library matches candidate commit');
is($primary->safe_psql('postgres', q{
SELECT count(*) FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace
WHERE n.nspname = 'pin' AND (p.proname ~ '^(g0_|g2_)' OR p.prosecdef OR p.proleakproof)
}), '0', 'no test SQL, definer privilege or leakproof promise');
for my $name ('enable_count_fastpath', 'enable_count_vm', 'enable_compact_reuse', 'enable_parallel_vacuum')
{
    is($primary->safe_psql('postgres', "SHOW pin.$name"), 'off', "$name remains default off");
}
is($primary->safe_psql('postgres', 'SHOW pin.parallel_count_workers'), '0', 'parallel count remains opt-in');
equivalent($primary, 'postgres', 'fresh installation');
my $base = snapshot_results($primary);

# online physical backup includes the native index pages and generic WAL.
command_ok(['pg_basebackup', '--pgdata=' . $primary->backup_dir . '/base',
    '--host=' . $primary->host, '--port=' . $primary->port,
    '--checkpoint=fast', '--wal-method=stream'], 'take a synchronized online backup')
    or BAIL_OUT('online backup failed');
command_ok(['pg_verifybackup', $primary->backup_dir . '/base'], 'physical backup manifest and WAL verify');
my $restored = PostgreSQL::Test::Cluster->new('g8_restored');
$restored->init_from_backup($primary, 'base');
$restored->start;
equivalent($restored, 'postgres', 'physical restore', $base);
$restored->stop;

# replay is supported independently of concurrent standby index reads.
my $standby = PostgreSQL::Test::Cluster->new('g8_standby');
$standby->init_from_backup($primary, 'base', has_streaming => 1);
$standby->start;
$primary->safe_psql('postgres', q{
BEGIN;
INSERT INTO docs VALUES (301, 'alpha beta', 0);
UPDATE docs SET body = 'gamma' WHERE id = 3;
UPDATE docs SET extra = 1 WHERE id = 9;
DELETE FROM docs WHERE id = 6;
COMMIT;
});
$primary->safe_psql('postgres', 'VACUUM docs');
$primary->wait_for_replay_catchup($standby);
my $target = snapshot_results($primary);
is(result($standby, 'postgres', 'alpha', 0), $target->{alpha}, 'standby sequential predicate is available');
rejected($standby, $bitmap . query_sql('alpha'), qr/0A000:.*during recovery/s,
    'standby Pin index execution fails closed');

# restore point follows committed publication, update and cleanup records.
$primary->safe_psql('postgres', "SELECT pg_create_restore_point('pin_g8_target')");
$primary->safe_psql('postgres', "INSERT INTO docs VALUES (302, 'alpha beta', 0)");
$primary->safe_psql('postgres', 'SELECT pg_switch_wal()');
my $pitr = PostgreSQL::Test::Cluster->new('g8_pitr');
$pitr->init_from_backup($primary, 'base', has_restoring => 1, standby => 0);
$pitr->append_conf('postgresql.conf', q{
recovery_target_name = 'pin_g8_target'
recovery_target_action = 'promote'
});
$pitr->start;
$pitr->poll_query_until('postgres', 'SELECT NOT pg_is_in_recovery()')
    or BAIL_OUT('PITR did not finish at the named target');
is($pitr->safe_psql('postgres', 'SELECT count(*) FROM docs WHERE id = 302'), '0', 'PITR excludes later commit');
equivalent($pitr, 'postgres', 'named-target PITR', $target);
$pitr->stop;

$primary->wait_for_replay_catchup($standby);
my $promoted = snapshot_results($primary);
is($standby->safe_psql('postgres', 'SELECT pg_promote(true, 30)'), 't', 'standby promotion completed');
equivalent($standby, 'postgres', 'promotion', $promoted);
$standby->safe_psql('postgres', "INSERT INTO docs VALUES (303, 'alpha beta', 0)");
equivalent($standby, 'postgres', 'post-promotion write');
$standby->stop;

# logical dumps reconstruct indexes through the authoritative extension SQL.
my $dump = $primary->basedir . '/pin.dump';
command_ok(['pg_dump', '--format=custom', '--file=' . $dump, '--dbname=' . $primary->connstr('postgres')], 'dump extension and table');
$primary->safe_psql('postgres', 'CREATE DATABASE restored');
command_ok(['pg_restore', '--exit-on-error', '--dbname=' . $primary->connstr('restored'), $dump], 'restore with matching installed extension');
equivalent($primary, 'restored', 'logical restore', $promoted);

for my $ddl ('REINDEX INDEX docs_pin', 'VACUUM FULL docs', 'CLUSTER docs USING docs_pkey',
    'BEGIN; TRUNCATE docs; ROLLBACK')
{
    $primary->safe_psql('postgres', $ddl);
    equivalent($primary, 'postgres', $ddl, $promoted);
}
rejected($primary, 'CREATE INDEX CONCURRENTLY docs_concurrent ON docs USING pin(body)',
    qr/0A000:.*nonconcurrent/s, 'concurrent build remains explicitly unsupported');
$primary->safe_psql('postgres', 'DROP INDEX IF EXISTS docs_concurrent');
rejected($primary, "ALTER EXTENSION pin UPDATE TO '0.0.1'", qr/no update path/, 'unreleased upgrade path is not invented');

# an ordinary role must retain RLS even when a superuser enables count tests.
$primary->safe_psql('postgres', q{
CREATE ROLE pin_reader;
GRANT USAGE ON SCHEMA pin TO pin_reader;
GRANT SELECT ON docs TO pin_reader;
ALTER TABLE docs ENABLE ROW LEVEL SECURITY;
CREATE POLICY readers ON docs TO pin_reader USING (id % 2 = 0);
});
my $role = 'SET ROLE pin_reader;';
my $rls_query = query_sql('alpha');
my $rls_expected = $primary->safe_psql('postgres', $sequential . $role . $rls_query);
is($primary->safe_psql('postgres', $bitmap . $role . $rls_query), $rls_expected, 'RLS bitmap and sequential rows agree');
my $count_sql = "SELECT count(*) FROM docs WHERE body OPERATOR(pin.@@@) pin.parse_query('alpha')";
my $rls_plan = $primary->safe_psql('postgres',
    'SET pin.enable_count_fastpath = on;' . $role . 'EXPLAIN (FORMAT JSON) ' . $count_sql);
unlike($rls_plan, qr/Custom Scan/, 'RLS count declines custom execution');
rejected($primary, $role . 'SET pin.enable_compact_reuse = on', qr/42501:/, 'ordinary role cannot enable retained-prefix compaction');
$primary->safe_psql('postgres', 'REVOKE SELECT ON docs FROM pin_reader');
rejected($primary, $role . $rls_query, qr/42501:/, 'revoked table permission is enforced');

$primary->safe_psql('postgres', q{
ALTER TABLE docs DISABLE ROW LEVEL SECURITY;
TRUNCATE docs;
INSERT INTO docs VALUES (1, 'beta gamma', 0), (2, '', 0), (3, NULL, 0);
});
$primary->safe_psql('postgres', 'VACUUM docs');
equivalent($primary, 'postgres', 'truncate and replacement identities');
is(result($primary, 'postgres', 'alpha', 1), '{}', 'old terms cannot survive truncate');
is(result($primary, 'postgres', 'NOT alpha', 1), '{1,2}', 'negation includes empty documents but not nulls');
$primary->stop('immediate');
$primary->start;
equivalent($primary, 'postgres', 'unclean restart');
$primary->safe_psql('postgres', 'DROP EXTENSION pin CASCADE');
is($primary->safe_psql('postgres', "SELECT count(*) FROM pg_am WHERE amname = 'pin'"), '0', 'drop removes access method');
is($primary->safe_psql('postgres', 'SELECT count(*) FROM docs'), '3', 'drop preserves heap source of truth');
$primary->safe_psql('postgres', 'CREATE EXTENSION pin; CREATE INDEX docs_pin ON docs USING pin(body)');
equivalent($primary, 'postgres', 'recreate after drop');
$primary->stop;
done_testing();
