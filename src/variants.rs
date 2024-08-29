use std::io;
use std::path::PathBuf;
use std::time::Instant;

use anyhow::Context;
use anyhow::Result;

use noodles::core::Position;
use noodles::vcf;
use noodles::vcf::variant::io::Write as _;
use noodles::vcf::variant::record::Ids as _;
use noodles::vcf::variant::record_buf;

use rusqlite::named_params;
use rusqlite::Connection;

// TODO: Use [sqlite_zstd](https://github.com/phiresky/sqlite-zstd?tab=readme-ov-file#usage)
// TODO: Deconstruct the INFO field into its own table (note that Values can be Arrays)

pub(crate) struct Variants {
    conn: Connection,
}

impl Variants {
    pub(crate) fn new(db: &PathBuf) -> Result<Self> {
        let conn = Connection::open(db)
            .with_context(|| format!("Failed to open database: {}", db.display()))?;

        // FIXME: Don't hardcode path
        const ZSTD_EXTENSION_PATH: &str =
            "/Users/gavrie/source/third_party/sqlite-zstd/target/release/libsqlite_zstd";

        unsafe {
            let _guard = rusqlite::LoadExtensionGuard::new(&conn)?;
            conn.load_extension(ZSTD_EXTENSION_PATH, None)
                .context("Failed to load libsqlite_zstd extension")?;
        }

        eprintln!("Opened database: {}", db.display());
        Ok(Self { conn })
    }

    pub(crate) fn import(&mut self, vcf_in: &PathBuf) -> Result<()> {
        let conn = &mut self.conn;

        eprintln!("Importing VCF: {}", vcf_in.display());

        // See: https://kerkour.com/sqlite-for-servers

        conn.execute_batch(
            r#"
            -- Recommended by zstd:
            PRAGMA journal_mode=WAL;
            PRAGMA auto_vacuum=full;
            PRAGMA busy_timeout=2000;

            BEGIN;

            CREATE TABLE IF NOT EXISTS variants (
                xrowid INTEGER PRIMARY KEY,
                chrom TEXT,
                pos INTEGER,
                id TEXT,
                ref TEXT,
                alt TEXT,
                qual REAL,
                filter TEXT,
                info TEXT
            );

            CREATE TABLE IF NOT EXISTS metadata (
                header TEXT
            );

            CREATE INDEX IF NOT EXISTS idx_variants_chrom_pos ON variants (chrom, pos);

            COMMIT;

            SELECT
                zstd_enable_transparent('{"table": "variants", "column": "info", "compression_level": 19, "dict_chooser": "''a''"}');

            -- How should we combine compression and insertion?
            -- Probably need to interleave this with 1000s of insertions.

            -- Compress everything as fast as possible:
            SELECT zstd_incremental_maintenance(NULL, 1);

            -- Spend 60 seconds compressing pending stuff, while allowing other queries to run 50% of the time.
            -- SELECT zstd_incremental_maintenance(60, 0.5);


            "#,
        )?;

        let mut reader = vcf::io::reader::Builder::default()
            .build_from_path(vcf_in)
            .with_context(|| format!("Failed to open VCF file: {}", vcf_in.display()))?;

        let header = reader.read_header()?;
        store_header(&header, conn)?;

        let before = Instant::now();
        let mut records_processed = 0;

        const SQL: &str = "
            INSERT INTO variants
                (chrom, pos, id, ref, alt, qual, filter, info)
            VALUES
                (:chrom, :pos, :id, :ref, :alt, :qual, :filter, :info)
            ";

        const BATCH_SIZE: usize = 10_000;

        let mut records = reader.records();
        let mut done = false;

        while !done {
            // Process a batch of records
            let tx = conn.transaction()?;
            {
                let mut stmt = tx.prepare(SQL)?;

                for _ in 0..BATCH_SIZE {
                    if let Some(result) = records.next() {
                        let record = result?;
                        records_processed += 1;
                        execute_record(record, &mut stmt)?;
                    } else {
                        done = true;
                        break;
                    }
                }

                let _: usize = tx
                    .query_row("SELECT zstd_incremental_maintenance(NULL, 1)", [], |row| {
                        row.get(0)
                    })
                    .context("Compress")?;
            }
            tx.commit()?;

            conn.execute("VACUUM", []).context("Vacuum")?;
            eprintln!("Records processed: {records_processed}");
        }

        conn.execute("ANALYZE", [])?;
        eprintln!("Import took {:.2?}", before.elapsed());
        Ok(())
    }

    pub(crate) fn query(
        &self,
        vcf_out: &PathBuf,
        group_by: Option<&str>,
        having: Option<&str>,
    ) -> Result<()> {
        let conn = &self.conn;

        let sql = "SELECT chrom, pos, id, ref, alt, qual, filter, info FROM variants".to_string();

        let sql = if let Some(group_by) = group_by {
            format!("{sql} GROUP BY {group_by}")
        } else {
            sql
        };

        let sql = if let Some(having) = having {
            format!("{sql} HAVING {having}")
        } else {
            sql
        };

        eprintln!("Exporting to VCF: {}", vcf_out.display());
        eprintln!("Query: {sql};");

        let before = Instant::now();
        let mut stmt = conn.prepare(&sql)?;
        let mut rows = stmt.query([])?;

        let mut writer = vcf::io::writer::Builder::default().build_from_path(vcf_out)?;
        let header = load_header(conn)?;
        writer.write_header(&header)?;

        while let Some(row) = rows.next()? {
            let chrom: String = row.get("chrom")?;
            let pos: Option<usize> = row.get("pos")?;
            let id: Option<String> = row.get("id")?;
            let ref_: String = row.get("ref")?;
            let alt: String = row.get("alt")?;
            let qual: Option<f32> = row.get("qual")?;
            let filter: String = row.get("filter")?;
            let info: String = row.get("info")?;

            // eprintln!("{chrom} {pos:?} {id:?} {ref_} {alt} {qual:?} {filter} {info}");

            // let optional_fields = bed::record::OptionalFields::from(vec![count.to_string()]);

            let pos = Position::try_from(pos.unwrap_or_default())?;
            let ids: record_buf::Ids = id.map(String::from).into_iter().collect();
            let alternate_bases = record_buf::AlternateBases::from(vec![alt]);
            let filters: record_buf::Filters = [filter].into_iter().collect();

            let info = parse_info(&info, &header)?;

            let mut record = vcf::variant::RecordBuf::builder()
                .set_reference_sequence_name(chrom)
                .set_variant_start(pos)
                .set_ids(ids)
                .set_reference_bases(ref_)
                .set_alternate_bases(alternate_bases)
                .set_filters(filters)
                // .set_info("BAR=QUUX".parse()?)
                .set_info(info)
                .build();

            // The builder doesn't accept an Option<f32> for the quality score,
            // so we have to set it afterwards.
            *record.quality_score_mut() = qual;

            writer.write_variant_record(&header, &record)?;
        }

        eprintln!("Query/export took {:.2?}", before.elapsed());

        Ok(())
    }
}

fn execute_record(
    record: vcf::Record,
    stmt: &mut rusqlite::Statement<'_>,
) -> Result<(), anyhow::Error> {
    let pos: Option<usize> = record.variant_start().transpose()?.map(usize::from);
    let ids = record.ids();
    let id: Option<&str> = ids.iter().next();
    let qual: Option<f32> = record.quality_score().transpose()?;

    stmt.execute(named_params! {
        ":chrom": record.reference_sequence_name(),
        ":pos": pos,
        ":id": id,
        ":ref": record.reference_bases(),
        ":alt": record.alternate_bases().as_ref(),
        ":qual": qual,
        ":filter": record.filters().as_ref(),
        ":info": record.info().as_ref(),
    })?;

    Ok(())
}

fn parse_info(info: &str, header: &vcf::Header) -> Result<record_buf::Info> {
    // TODO: There seems to be no way to set the info from a raw string
    // (like the one we kept when reading the VCF).
    // It seems we must parse the string and reconstruct it here :shrug:
    //
    // Ideas:
    // - Use the original string parsing from the vcf reader to parse the INFO from the DB
    // - Store the entire record in the DB and export it here.
    //   The INFO field is the largest in any case, so this may not be a problem.

    use record_buf::info::field::Value;

    // let ns = (String::from("FOO"), Some(Value::String("BAR".to_string())));
    // let info: record_buf::Info = [ns].into_iter().collect();

    let info = vcf::record::Info::new(info);
    let info: io::Result<Vec<_>> = info.iter(header).collect();
    let info = info?;
    let info: Vec<(String, Option<Value>)> = info
        .into_iter()
        .map(|(k, v)| {
            let v: Option<Value> = v.map(|v| v.try_into().unwrap());
            (k.to_string(), v)
        })
        .collect();
    let info: record_buf::Info = info.into_iter().collect();

    // eprintln!("{info:#?}");
    // std::process::exit(0);

    // let info: std::io::Result<Vec<_>> = info.iter(&header).collect();
    // let info = record_buf::Info::from(info); // = info.iter(&header).collect();

    Ok(info)
}

fn store_header(header: &vcf::Header, conn: &Connection) -> Result<()> {
    let header = {
        let mut buf = io::Cursor::new(Vec::new());
        {
            let mut writer = vcf::io::writer::Builder::default().build_from_writer(&mut buf);
            writer.write_header(header)?;
        }
        String::from_utf8(buf.into_inner()).context("Failed to convert header to string")?
    };

    conn.execute(
        "INSERT INTO metadata (header) VALUES (:header)",
        named_params! {
            ":header": header,
        },
    )?;

    Ok(())
}

fn load_header(conn: &Connection) -> Result<vcf::Header> {
    let header: String = conn.query_row("SELECT header FROM metadata", [], |row| row.get(0))?;

    let mut reader =
        vcf::io::reader::Builder::default().build_from_reader(io::Cursor::new(header))?;

    let header = reader.read_header()?;

    Ok(header)
}
