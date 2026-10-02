-- villagerdb's items, in miniature: one JSON document per item, and the
-- directory is the table. Two prices live inside `games`, one per game; a
-- pointer says where, and a member that does not describe that game reads
-- null there.
CREATE TABLE items (
  id       TEXT   NOT NULL,
  name     TEXT   NOT NULL,
  category TEXT   NOT NULL,
  nh_sell  BIGINT OPTIONS(matches = 'games', pointer = '/nh/sellPrice/value'),
  nl_sell  BIGINT OPTIONS(matches = 'games', pointer = '/nl/sellPrice/value')
)
WITH (files = '*.json');
