# mercurio-storage

Persistence for the [Mercurio](https://github.com/frenzox/mercurio) MQTT broker: sessions, subscriptions,
retained messages, wills, in-flight messages and offline queues.

It defines `BatchStore` for what the broker keeps per client,
`RetainedMessageStore` for retained messages and `BanStore` for bans,
the first two gathered in `MqttStore`, with three implementations:

- `memory::MemoryStore`: everything in memory, lost on restart.
- `sqlite::SqliteStore`: a SQLite database, behind the `sqlite` feature.
- `postgres::PostgresStore`: a PostgreSQL database, behind the `postgres`
  feature, over TLS, required unless the server is on the same host. A
  broker serving from it opens it with
  `PostgresStore::connect_exclusive(url, &options, wait)`, which keeps any
  other broker from writing to the database meanwhile.

```rust,ignore
use std::sync::Arc;

use mercurio_server::server::{ListenerConfig, Server, Transport};
use mercurio_storage::sqlite::SqliteStore;

let storage = Arc::new(SqliteStore::new("/var/lib/myapp/mqtt.db")?);
Server::builder()
    .storage(storage)
    .listener(ListenerConfig::new(listener, Transport::Mqtt))
    .run(shutdown)
    .await?;
```

Implement the traits to keep the broker's state anywhere else. The broker
writes each client's changes through `BatchStore::apply`, in batches,
which is the one method that writes; stores that can should apply a batch
in one transaction. The other methods read. The `test-util` feature adds
`testing::conformance`, the suite the three stores here pass: run it
against a store of your own from its tests.

The broker retries a batch that fails with `StorageError::Transient`, such
as on a lost connection, until storage recovers, and gives up others after
a few attempts. `StorageError::Fenced` means another broker took the
database over: the broker drops the batch, and should stop.

## Features

- `sqlite`: the SQLite store, with schema migrations applied on open.
- `postgres`: the PostgreSQL store, for PostgreSQL 12 or later, which
  creates and updates its tables on connect. Its tests run when
  `MERCURIO_TEST_POSTGRES_URL` names a database they may empty, set when
  they are built; without it they are listed as ignored.
- `test-util`: the `testing` module.

## License

MIT or Apache-2.0, at your option.
