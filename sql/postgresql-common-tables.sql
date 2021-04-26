CREATE TABLE levels (
        id SERIAL PRIMARY KEY,
        _level INTEGER NOT NULL,
        hash VARCHAR(60));


CREATE UNIQUE INDEX levels__level ON levels(_level);
CREATE UNIQUE INDEX levels_hash ON levels(hash);

CREATE TABLE max_id (
       max_id INT4
);

INSERT INTO max_id (max_id) VALUES (1);
