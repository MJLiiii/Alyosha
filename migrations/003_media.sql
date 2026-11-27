-- Images people send, and video frames, by the hash of their bytes:
-- docs/Media.md#the-media-table
create table media (
    sha256      text primary key,
    mime        text not null,
    bytes       bytea not null,
    created_at  timestamptz not null default now()
);
