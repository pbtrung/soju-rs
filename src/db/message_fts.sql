CREATE VIRTUAL TABLE MessageFTS USING fts5 (
    text, content=Message, content_rowid=id, columnsize=0
);
CREATE TRIGGER MessageFTSInsert AFTER INSERT ON Message
WHEN new.text IS NOT NULL BEGIN
    INSERT INTO MessageFTS(rowid, text) VALUES (new.id, new.text);
END;
CREATE TRIGGER MessageFTSDelete AFTER DELETE ON Message
WHEN old.text IS NOT NULL BEGIN
    INSERT INTO MessageFTS(MessageFTS, rowid, text) VALUES ('delete', old.id, old.text);
END;
CREATE TRIGGER MessageFTSUpdate AFTER UPDATE ON Message
WHEN old.id IS NOT new.id OR old.text IS NOT new.text BEGIN
    INSERT INTO MessageFTS(MessageFTS, rowid, text)
        SELECT 'delete', old.id, old.text WHERE old.text IS NOT NULL;
    INSERT INTO MessageFTS(rowid, text)
        SELECT new.id, new.text WHERE new.text IS NOT NULL;
END;
