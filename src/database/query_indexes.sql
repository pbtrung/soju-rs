-- One timestamp index entry per message, split by history playback mode.
CREATE INDEX IF NOT EXISTS MessageEventIndex ON Message(target, time) WHERE text IS NULL;
DROP INDEX IF EXISTS MessageIndex;
-- Unique indexes already cover these leading-column lookups and FK checks.
DROP INDEX IF EXISTS Network_user_index;
DROP INDEX IF EXISTS Channel_network_index;
DROP INDEX IF EXISTS DeliveryReceipt_network_index;
DROP INDEX IF EXISTS ReadReceipt_network_index;
DROP INDEX IF EXISTS MessageTarget_network_index;
DROP INDEX IF EXISTS WebPushSubscription_network_index;
DROP INDEX IF EXISTS WebPushSubscription_user_index;
CREATE INDEX IF NOT EXISTS DeliveryReceipt_client_index ON DeliveryReceipt(network, client);
CREATE INDEX IF NOT EXISTS WebPushSubscription_user_network_index ON WebPushSubscription(user, network);
CREATE INDEX IF NOT EXISTS DeviceCertificate_user_index ON DeviceCertificate(user);
CREATE INDEX IF NOT EXISTS MessageTarget_flags_index ON MessageTarget(network) WHERE pinned OR muted OR blocked;
