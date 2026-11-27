-- Memory and people's notes: docs/Memory.md#implementation
-- The memory files of earlier builds are imported in the same transaction:
-- docs/Memory.md#import-the-old-files
create table memories (
    name         text primary key,
    description  text not null,
    body         text not null,
    updated      date not null,
    version      bigint not null default 1
);

create table user_notes (
    person   text primary key,           -- telegram-123456789, qq-<member_openid>
    name     text not null,
    notes    text not null,
    updated  date not null,
    version  bigint not null default 1
);
