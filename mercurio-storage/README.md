# mercurio-storage

Persistence for the [Mercurio](https://github.com/frenzox/mercurio) MQTT broker: sessions, subscriptions,
retained messages, wills, in-flight messages and offline queues.

It defines one trait per kind of state, gathered in `MqttStore`, with three
implementations:

- `memory::MemoryStore`: everything in memory, lost on restart.
- `sqlite::SqliteStore`: a SQLite database, behind the `sqlite` feature.
- `postgres::PostgresStore`: a PostgreSQL database, behind the `postgres`
  feature, over TLS when the server supports it. A broker serving from it
  opens it with `PostgresStore::connect_exclusive(url, ca_path, wait)`,
  which keeps any other broker from writing to the database meanwhile.

```rust,ignore
use std::sync::Arc;

use mercurio_server::server::{run_with_storage, ServerConfig};
use mercurio_storage::sqlite::SqliteStore;

let storage = Arc::new(SqliteStore::new("/var/lib/myapp/mqtt.db")?);
run_with_storage(listener, ServerConfig::default(), storage, shutdown).await?;
```

Implement the traits to keep the broker's state anywhere else. The broker
writes each client's changes through `BatchStore::apply`, in batches; its
default applies them one at a time, and stores that can should apply a
batch in one transaction. A store overriding it can pass the changes it
has no batch form for, including kinds added in later versions, to
`apply_each`.

The broker retries a batch that fails with `StorageError::Transient`, such
as on a lost connection, until storage recovers, and gives up others after
a few attempts. `StorageError::Fenced` means another broker took the
database over: the broker drops the batch, and should stop.

## Features

- `sqlite`: the SQLite store, with schema migrations applied on open.
- `postgres`: the PostgreSQL store, for PostgreSQL 12 or later, which
  creates and updates its tables on connect.

## License

MIT or Apache-2.0, at your option.
