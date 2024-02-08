-- Add migration script here
create table if not exists websites (
    id serial primary key,
    url varchar not null,
    alias varchar(75) not null unique
);

create table if not exists logs (
    id serial primary key,
    website_alias varchar(75) not null references websites(alias),
    status smallint,
    created_at timestamp with time zone not null default date_trunc('minute', current_timestamp),
    UNIQUE (website_alias, created_at) 
);
