#!/usr/bin/env nu

$env.config.use_ansi_coloring = true; 

def trunc [n: int]: list -> list {
  let l = $in
  if ($l | length) > $n {
    ($l | first $n) | append "…"
  } else {
    $l
  }
}

ralph status --json
| from json
| where phase != Done and ($it | get -o postmortem) == null
| select id title phase phase_ordinal blocked_by attempts
| rename index
| update blocked_by {trunc 3 | str join " "}
| update phase_ordinal {0 - $in}
| sort-by phase_ordinal blocked_by
| reject phase_ordinal
| table
