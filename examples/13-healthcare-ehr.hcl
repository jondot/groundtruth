# EHR: integrity is patient-safety critical, HIPAA needs an unbroken PHI audit trail
# integrity fails page on-call + compliance

connection "postgres" "ehr" {
  dsn = env("DATABASE_URL")
}

state {
  dsn = env("DATABASE_URL")
}

defaults {
  on      = connection.postgres.ehr
  every   = "5m"
  on_fail = notify.webhook.oncall
}

notify "webhook" "oncall" {
  url = env("ALERT_WEBHOOK")
}

check "encounters_have_valid_patient" {
  query = <<-SQL
    select e.id, e.patient_id
    from encounters e
    left join patients p on p.id = e.patient_id
    where p.id is null
  SQL
  fail {
    when   = rows.count > 0
    sample = 10
  }
}

check "lab_results_have_valid_encounter" {
  query = <<-SQL
    select l.id, l.encounter_id
    from lab_results l
    left join encounters e on e.id = l.encounter_id
    where e.id is null
  SQL
  fail {
    when   = rows.count > 0
    sample = 10
  }
}

# med order needs a real patient AND a real prescriber
check "medication_orders_valid" {
  query = <<-SQL
    select m.id
    from medication_orders m
    left join patients p on p.id = m.patient_id
    left join providers pr on pr.id = m.prescriber_id
    where p.id is null or pr.id is null
  SQL
  fail {
    when   = rows.count > 0
    sample = 10
  }
}

check "patient_demographics_complete" {
  query = <<-SQL
    select mrn, date_of_birth, sex
    from patients
    where created_at > now() - interval '1 day'
  SQL
  validate {
    column "mrn" {
      not_null = true
      unique   = true
    }
    column "date_of_birth" {
      type     = "timestamp"
      not_null = true
    }
    column "sex" {
      allowed = ["male", "female", "other", "unknown"]
    }
  }
}

# two active records on one MRN = merge error splitting a patient's history
check "no_duplicate_active_mrn" {
  query = <<-SQL
    select mrn, count(*) as n
    from patients
    where status = 'active'
    group by mrn
    having count(*) > 1
  SQL
  fail {
    when   = rows.count > 0
    sample = 10
  }
}

# impossible vitals = data-entry or device errors
check "vitals_in_physiologic_range" {
  query = <<-SQL
    select heart_rate, temp_c, spo2
    from vitals
    where recorded_at > now() - interval '1 hour'
  SQL
  validate {
    column "heart_rate" {
      type  = "int"
      range = { min = 20, max = 250 }
    }
    column "temp_c" {
      type  = "float"
      range = { min = 25, max = 45 }
    }
    column "spo2" {
      type  = "int"
      range = { min = 50, max = 100 }
    }
  }
}

# open 48h+ = forgotten, not ongoing
check "stale_open_encounters" {
  query = <<-SQL
    select count(*) as n
    from encounters
    where status = 'open'
      and opened_at < now() - interval '48 hours'
  SQL
  warn = row.n > 0
}

# critical result unacked for 1h needs escalation
check "unacknowledged_critical_labs" {
  query = <<-SQL
    select id, encounter_id, resulted_at
    from lab_results
    where flag = 'critical'
      and acknowledged_at is null
      and resulted_at < now() - interval '1 hour'
  SQL
  fail {
    when   = rows.count > 0
    sample = 10
  }
}

# a gap in the PHI log = unlogged access, a breach in itself
check "phi_audit_log_fresh" {
  query = "select extract(epoch from now() - max(accessed_at))::float8 as age from phi_access_log"
  fail  = row.age > duration("15m")
}

# every access must name who
check "audit_entries_attributed" {
  query = <<-SQL
    select count(*) as n
    from phi_access_log
    where user_id is null
      and accessed_at > now() - interval '1 hour'
  SQL
  fail = row.n > 0
}

# active patient should have a current consent record
check "active_patients_have_consent" {
  query = <<-SQL
    select count(*) as n
    from patients p
    left join consents c on c.patient_id = p.id and c.status = 'active'
    where p.status = 'active' and c.id is null
  SQL
  warn = row.n > 0
}
