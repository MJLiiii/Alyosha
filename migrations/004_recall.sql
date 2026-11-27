-- Recall of past sessions: docs/Recall.md#schema-and-text
-- pgvector is not a trusted extension: the database service creates it as
-- postgres, so this is a no-op in the container.
create extension if not exists vector;

-- The searchable text of each entry. Migration 4 fills it for existing rows.
alter table history add column text text;

create table history_embeddings (
    session_id  bigint not null,
    seq         bigint not null,
    model       text not null,
    embedding   vector(1536) not null,
    primary key (session_id, seq, model),
    foreign key (session_id, seq) references history
);
