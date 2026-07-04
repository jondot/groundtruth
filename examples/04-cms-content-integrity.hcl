# Headless CMS integrity: media pointing at hard-deleted posts 500s the page.
# Offending rows are attached to the report so the editor can fix exact records.

connection "postgres" "cms" {
  dsn = env("DATABASE_URL")
}

defaults {
  on    = connection.postgres.cms
  every = "10m"
}

# no live posts = blank site
check "published_posts_exist" {
  query = "select count(*) as n from posts where status = 'published'"
  fail  = row.n == 0
}

# media whose post_id no longer resolves; sample so the editor sees which assets
check "no_orphaned_media" {
  query = <<-SQL
    select m.id, m.filename, m.post_id
    from media m
    left join posts p on p.id = m.post_id
    where p.id is null
  SQL
  fail {
    when   = rows.count > 0
    sample = 10
  }
}
