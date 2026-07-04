# Edge read-replica cache: a stalled sync silently serves stale data to shoppers.
# The replica is the read path; if its sync log falls behind, prices go stale.

connection "postgres" "replica" {
  dsn = env("REPLICA_URL")
}

defaults {
  on    = connection.postgres.replica
  every = "1m"
}

# newest sync_log stamp older than 10m = replica drifting out of sync
check "cache_is_fresh" {
  query = "select extract(epoch from now() - max(synced_at)) as age from sync_log"
  warn  = row.age > duration("10m")
  fail  = row.age > duration("30m")
}

# no products = first sync never landed, storefront is unusable
check "catalog_not_empty" {
  query = "select count(*) as n from products"
  fail  = row.n == 0
}
