//! Deterministic semantic properties; failures print their seed and case.
mod support;

use soju::conn::Conn;
use soju::irc::{Message, Prefix, Tags};
use soju::logger::Logger;
use soju::service::{format_go_duration, parse_go_duration};
use soju::{msg, xirc};
use support::{Generator, SEEDS};
use tokio::io::AsyncWriteExt;

#[test]
fn tag_escaping_is_reversible_for_generated_unicode() -> anyhow::Result<()> {
    for seed in SEEDS {
        let mut generator = Generator(seed);
        for case in 0..512 {
            let value = generator.text(case % 129)?;
            let mut tags = Tags::new();
            tags.set("+property/value", &value);
            tags.set("duplicate", "old");
            tags.set("duplicate", &value);
            let wire = tags.to_string();
            assert_eq!(Tags::parse(&wire), tags, "seed={seed:x} case={case}");
            let message = msg!("PRIVMSG", "#test", "text").with_tags(tags);
            assert_eq!(
                Message::parse(&message.to_string())?,
                message,
                "seed={seed:x} case={case}"
            );
        }
    }
    Ok(())
}

#[test]
fn message_format_parse_retains_valid_fields() -> anyhow::Result<()> {
    for seed in SEEDS {
        let mut generator = Generator(seed);
        for case in 0..512 {
            let text = generator.text(case % 129)?.replace(['\r', '\n'], " ");
            let message =
                msg!("PRIVMSG", "#room", text).with_prefix(Prefix::full("nick", "user", "host"));
            let wire = message.to_string();
            let parsed = Message::parse(&wire)?;
            assert_eq!(parsed, message, "seed={seed:x} case={case}");
            assert_eq!(parsed.to_string(), wire);
        }
    }
    Ok(())
}

#[test]
fn multiline_reassembly_preserves_utf8_and_blank_lines() -> anyhow::Result<()> {
    for seed in SEEDS {
        let mut generator = Generator(seed);
        for case in 0..256 {
            let text = generator.text(case % 193)?.replace('\r', "");
            let message = msg!("PRIVMSG", "#room", &text).with_tag("msgid", "property");
            let batch = xirc::generate_multiline_batch(&message, "property", 64 + case % 97);
            let opener = batch
                .first()
                .ok_or_else(|| anyhow::anyhow!("missing opener"))?;
            let mut assembler = xirc::MultilineBatch::new(opener.clone());
            for part in batch.iter().skip(1).take(batch.len().saturating_sub(2)) {
                assert!(
                    part.param(1).len() <= 64 + case % 97,
                    "seed={seed:x} case={case}"
                );
                assembler.push(part, &xirc::MultilineLimits::default());
            }
            let normalized = text.trim_end_matches('\n');
            if normalized.is_empty() {
                assert_eq!(
                    assembler.finish().err(),
                    Some(xirc::MultilineError::Invalid)
                );
                continue;
            }
            let rebuilt = assembler
                .finish()
                .map_err(|error| anyhow::anyhow!("seed={seed:x} case={case}: {}", error.code()))?;
            assert_eq!(rebuilt.param(1), normalized, "seed={seed:x} case={case}");
            assert_eq!(rebuilt.tags.get("msgid"), Some("property"));
        }
    }
    Ok(())
}

#[test]
fn duration_format_parse_preserves_nanoseconds() {
    for seed in SEEDS {
        let mut generator = Generator(seed);
        for case in 0..2048 {
            let nanos = generator.next() & (u64::MAX >> 1);
            let duration = std::time::Duration::from_nanos(nanos);
            assert_eq!(
                parse_go_duration(&format_go_duration(duration)),
                Some(duration),
                "seed={seed:x} case={case}"
            );
        }
    }
    for invalid in ["-1ns", "18446744073709551616ns", "NaNs", "1.2.3s"] {
        assert!(parse_go_duration(invalid).is_none(), "{invalid}");
    }
}

#[tokio::test]
async fn fragmented_utf8_and_lossy_octets_preserve_frames() -> anyhow::Result<()> {
    for chunk_size in 1..=17 {
        let (input, mut output) = tokio::io::duplex(32);
        let (connection, mut reader) = Conn::new_stream(Box::new(input), Logger::new(false), None);
        let producer = tokio::spawn(async move {
            let mut bytes = "PRIVMSG #room :é中🦀\r\n".as_bytes().to_vec();
            bytes.extend_from_slice(b"PRIVMSG #room :\xff\xc3\x28\r\nPING :end\r\n");
            for chunk in bytes.chunks(chunk_size) {
                output.write_all(chunk).await?;
            }
            output.shutdown().await
        });
        let first = reader
            .read_message()
            .await?
            .ok_or_else(|| anyhow::anyhow!("missing first frame"))?;
        assert_eq!(first.param(1), "é中🦀", "chunk={chunk_size}");
        let second = reader
            .read_message()
            .await?
            .ok_or_else(|| anyhow::anyhow!("missing second frame"))?;
        assert_eq!(second.param(1), "��(", "chunk={chunk_size}");
        assert_eq!(
            reader.read_message().await?.map(|message| message.command),
            Some("PING".to_owned())
        );
        assert!(reader.read_message().await?.is_none());
        producer.await??;
        connection.close();
    }
    Ok(())
}
