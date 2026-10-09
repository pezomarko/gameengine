-- The item bar (LOOK.md 3.2, ITEMS.md 4): a character's arrangement of the stacks it uses
-- with a key, one row a character once it has arranged it, a template key or nothing in
-- each of the four cells. Without a row the hub answers the default (the first stack that
-- heals, on the first cell). The key is the content's: a template that leaves the content
-- stays here as a name and counts nothing.
create table bars (
    character_id bigint primary key references characters(id) on delete cascade,
    cell_1 text,
    cell_2 text,
    cell_3 text,
    cell_4 text
);
