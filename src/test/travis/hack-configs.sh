#! /bin/bash

PG=$1

cat << DONE >> /etc/postgresql/${PG}/main/postgresql.conf
client_min_messages=warning
autovacuum=off
fsync=off
zdb.default_elasticsearch_url = 'http://localhost:9200/'
zdb.log_level = LOG
zdb.default_replicas = 0
DONE

