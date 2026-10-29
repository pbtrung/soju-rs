INSERT INTO Message(id, target, raw, time, sender, text)
                 SELECT :id, id, CAST(:raw AS TEXT), :time, :sender, CAST(:text AS TEXT)
                 FROM MessageTarget WHERE network=:network AND target=:target
                 ON CONFLICT(id) DO UPDATE SET raw = CASE
                   WHEN Message.target=excluded.target AND Message.raw=excluded.raw
                    AND Message.time=excluded.time AND Message.sender=excluded.sender
                    AND Message.text IS excluded.text THEN Message.raw ELSE NULL END
