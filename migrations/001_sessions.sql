-- Durable sessions: docs/develop/SessionStore.md#schema
create table sessions (
    id          bigserial primary key,
    key         text not null,
    cwd         text not null,
    created_at  timestamptz not null default now(),
    updated_at  timestamptz not null default now()
);
create index sessions_key on sessions (key, id desc);

create table history (
    session_id  bigint not null references sessions,
    seq         bigint not null,
    kind        text not null,
    message_id  bigint,
    entry       json not null,         -- json, not jsonb: kept exactly as written
    created_at  timestamptz not null default now(),
    primary key (session_id, seq)
);

create table checkpoints (
    session_id      bigint not null references sessions,
    first_kept_seq  bigint not null,
    summary         text not null,
    created_at      timestamptz not null default now(),
    primary key (session_id, first_kept_seq)
);
