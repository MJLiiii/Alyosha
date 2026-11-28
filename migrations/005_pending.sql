-- The messages waiting in each session's queue, listed after a restart:
-- docs/SessionStore.md#the-waiting-queue
create table pending (
    session_id    bigint not null references sessions,
    message_id    bigint not null,
    submitted_at  timestamptz not null,
    text          text not null,
    primary key (session_id, message_id)
);
