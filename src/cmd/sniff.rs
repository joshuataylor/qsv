static USAGE: &str = r#"
Quickly sniff and infer CSV metadata (delimiter, header row, number of preamble rows,
quote character, flexible, is_utf8, number of records, file size, number of fields,
field names & data types) using a Viterbi algorithm 
(https://en.wikipedia.org/wiki/Viterbi_algorithm).

NOTE: This command "sniffs" a CSV's schema by sampling the first n rows of a file.
Its inferences are sometimes wrong if the sample is not large enough (use --sample 
to adjust). 

If you want more robust, guaranteed schemata, use the "schema" or "stats" commands
instead as they scan the entire file.

For examples, see https://github.com/jqnatividad/qsv/blob/master/tests/test_sniff.rs.

Usage:
    qsv sniff [options] [<input>]
    qsv sniff --help

sniff arguments:
    <input>                  The CSV to sniff. This can be a local file, stdin 
                             or a URL (http and https schemes supported).

                             Note that when input is a URL, sniff will automatically
                             download the file to a temporary file and sniff it. It
                             will create the file using csv::QuoteStyle::NonNumeric
                             so the sniffed schema may not be the same as the original.
                             This is done to increase the chances of sniffing the
                             correct schema.

sniff options:
    --sample <size>          First n rows to sample to sniff out the metadata.
                             When sample size is between 0 and 1 exclusive, 
                             it is treated as a percentage of the CSV to sample
                             (e.g. 0.20 is 20 percent).
                             When it is zero, the entire file will be sampled.
                             When the input is a URL, the sample size dictates
                             how many lines to sample without having to
                             download the entire file.
                             [default: 1000]
    --prefer-dmy             Prefer to parse dates in dmy format.
                             Otherwise, use mdy format.
    --json                   Return results in JSON format.
    --pretty-json            Return results in pretty JSON format.
    --save-urlsample <file>  Save the URL sample to a file.
                             Valid only when input is a URL.
    --timeout <secs>         Timeout for URL requests in seconds.
                             [default: 30]

Common options:
    -h, --help               Display this message
    -d, --delimiter <arg>    The field delimiter for reading CSV data.
                             Specify this when the delimiter is known beforehand,
                             as the delimiter guessing algorithm can sometimes be
                             wrong if not enough delimiters are present in the sample.
                             Must be a single ascii character.
    -p, --progressbar        Show progress bars. Only valid for URL input.
"#;

use std::{cmp::min, fmt, fs, io::Write, time::Duration};

use bytes::Bytes;
use futures::executor::block_on;
use futures_util::StreamExt;
use indicatif::{HumanCount, ProgressBar, ProgressDrawTarget, ProgressStyle};
use qsv_sniffer::{DatePreference, SampleSize, Sniffer};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tabwriter::TabWriter;
use tempfile::NamedTempFile;
use thousands::Separable;
use url::Url;

use crate::{
    config::{Config, Delimiter},
    util, CliResult,
};

#[derive(Deserialize)]
struct Args {
    arg_input:           Option<String>,
    flag_sample:         f64,
    flag_prefer_dmy:     bool,
    flag_json:           bool,
    flag_save_urlsample: Option<String>,
    flag_pretty_json:    bool,
    flag_delimiter:      Option<Delimiter>,
    flag_progressbar:    bool,
    flag_timeout:        u64,
}

#[derive(Serialize, Deserialize, Default, Debug)]
struct SniffStruct {
    path:            String,
    sniff_timestamp: String,
    delimiter_char:  char,
    header_row:      bool,
    preamble_rows:   usize,
    quote_char:      String,
    flexible:        bool,
    is_utf8:         bool,
    retrieved_size:  usize,
    file_size:       usize,
    sampled_records: usize,
    estimated:       bool,
    num_records:     usize,
    avg_record_len:  usize,
    num_fields:      usize,
    fields:          Vec<String>,
    types:           Vec<String>,
}
impl fmt::Display for SniffStruct {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        writeln!(f, "Path: {}", self.path)?;
        writeln!(f, "Sniff Timestamp: {}", self.sniff_timestamp)?;
        writeln!(
            f,
            "Delimiter: {}",
            if self.delimiter_char == '\t' {
                "tab".to_string()
            } else {
                self.delimiter_char.to_string()
            }
        )?;
        writeln!(f, "Header Row: {}", self.header_row)?;
        writeln!(
            f,
            "Preamble Rows: {}",
            self.preamble_rows.separate_with_commas()
        )?;
        writeln!(f, "Quote Char: {}", self.quote_char)?;
        writeln!(f, "Flexible: {}", self.flexible)?;
        writeln!(f, "Is UTF8: {}", self.is_utf8)?;
        writeln!(
            f,
            "Retrieved Size (bytes): {}",
            self.retrieved_size.separate_with_commas()
        )?;
        writeln!(
            f,
            "File Size (bytes): {}",
            self.file_size.separate_with_commas()
        )?;
        writeln!(
            f,
            "Sampled Records: {}",
            self.sampled_records.separate_with_commas()
        )?;
        writeln!(f, "Estimated: {}", self.estimated)?;
        writeln!(
            f,
            "Num Records: {}",
            self.num_records.separate_with_commas()
        )?;
        writeln!(
            f,
            "Avg Record Len (bytes): {}",
            self.avg_record_len.separate_with_commas()
        )?;
        writeln!(f, "Num Fields: {}", self.num_fields.separate_with_commas())?;
        writeln!(f, "Fields:")?;

        let mut tabwtr = TabWriter::new(vec![]);

        for (i, ty) in self.types.iter().enumerate() {
            writeln!(
                &mut tabwtr,
                "\t{}:\t{}\t{}",
                i,
                ty,
                self.fields.get(i).unwrap_or(&String::new())
            )
            .unwrap_or_default();
        }
        tabwtr.flush().unwrap();

        let tabbed_field_list = String::from_utf8(tabwtr.into_inner().unwrap()).unwrap();
        writeln!(f, "{tabbed_field_list}")?;

        Ok(())
    }
}

#[derive(Debug, Clone)]
struct SniffFileStruct {
    display_path:       String,
    file_to_sniff:      String,
    tempfile_flag:      bool,
    retrieved_size:     usize,
    file_size:          usize,
    downloaded_records: usize,
}

const fn rowcount(
    metadata: &qsv_sniffer::metadata::Metadata,
    sniff_file_info: &SniffFileStruct,
    count: usize,
) -> (usize, bool) {
    let mut estimated = false;
    let rowcount = if count == usize::MAX {
        // if the file is usize::MAX, it's a sentinel value for "Unknown" as the server
        // didn't provide a Content-Length header, so we estimate the rowcount by
        // dividing the file_size by avg_rec_len
        estimated = true;
        sniff_file_info.file_size / metadata.avg_record_len
    } else {
        count
    };

    let has_header_row = metadata.dialect.header.has_header_row;
    let num_preamble_rows = metadata.dialect.header.num_preamble_rows;
    let mut final_rowcount = rowcount;

    if !has_header_row {
        final_rowcount += 1;
    }

    final_rowcount -= num_preamble_rows;
    (final_rowcount, estimated)
}

async fn get_file_to_sniff(args: &Args) -> CliResult<SniffFileStruct> {
    if let Some(uri) = args.arg_input.clone() {
        match uri {
            // its a URL, download sample to temp file
            url if Url::parse(&url).is_ok() && url.starts_with("http") => {
                let client = match Client::builder()
                    .user_agent(util::DEFAULT_USER_AGENT)
                    .brotli(true)
                    .gzip(true)
                    .deflate(true)
                    .use_rustls_tls()
                    .http2_adaptive_window(true)
                    .build()
                {
                    Ok(c) => c,
                    Err(e) => {
                        return fail_clierror!("Cannot build reqwest client: {e}.");
                    }
                };

                let res = client
                    .get(url.clone())
                    .timeout(Duration::from_secs(args.flag_timeout))
                    .send()
                    .await
                    .or(Err(format!("Failed to GET from '{url}'")))?;

                let total_size = match res.content_length() {
                    Some(l) => l as usize,
                    None => {
                        // if we can't get the content length, just set it to a large value
                        // so we just end up downloading the entire file
                        usize::MAX
                    }
                };

                #[allow(clippy::cast_precision_loss)]
                let lines_sample_size = if args.flag_sample > 1.0 {
                    args.flag_sample.round() as usize
                } else if args.flag_sample.abs() < f64::EPSILON {
                    // sample size is zero, so we want to download the entire file
                    usize::MAX
                } else {
                    // sample size is a percentage, download percentage number of lines
                    // from the file. Since we don't know how wide the lines are, we
                    // just download a percentage of the bytes, assuming the lines are
                    // 100 characters wide as a rough estimate.
                    ((total_size / 100_usize) as f64 * args.flag_sample) as usize
                };

                // prep progress bar
                let show_progress =
                    args.flag_progressbar || std::env::var("QSV_PROGRESSBAR").is_ok();

                let progress = ProgressBar::with_draw_target(
                    Some(total_size.try_into().unwrap_or(u64::MAX)),
                    ProgressDrawTarget::stderr_with_hz(5),
                );
                if show_progress {
                    progress.set_style(
                        ProgressStyle::default_bar()
                            .template(
                                "{msg}\n{spinner:.green} [{elapsed_precise}] \
                                 [{wide_bar:.white/blue}] {bytes}/{total_bytes} ({bytes_per_sec}, \
                                 {eta})",
                            )
                            .unwrap(),
                    );
                    progress.set_message(format!(
                        "Downloading {} samples...",
                        HumanCount(lines_sample_size as u64)
                    ));
                } else {
                    progress.set_draw_target(ProgressDrawTarget::hidden());
                }

                let mut file = NamedTempFile::new()?;
                let mut downloaded = 0_usize;
                let mut stream = res.bytes_stream();
                let mut downloaded_lines = 0_usize;
                #[allow(unused_assignments)]
                let mut chunk = Bytes::new(); // amortize the allocation

                // download chunks until we have the desired sample size
                while let Some(item) = stream.next().await {
                    chunk = item.or(Err("Error while downloading file".to_string()))?;
                    file.write_all(&chunk)
                        .map_err(|_| "Error while writing to file".to_string())?;
                    let chunk_len = chunk.len();
                    downloaded = min(downloaded + chunk_len, total_size);
                    if show_progress {
                        progress.inc(chunk_len as u64);
                    }

                    // scan chunk for newlines
                    let num_lines = chunk.into_iter().filter(|&x| x == b'\n').count();
                    // and keep track of the number of lines downloaded which is ~= sample_size
                    downloaded_lines += num_lines;
                    // we downloaded enough samples, stop downloading
                    if downloaded_lines > lines_sample_size {
                        break;
                    }
                }
                drop(client);

                // we subtract 1 because we don't want to count the header row
                downloaded_lines -= 1;

                if show_progress {
                    progress.finish_with_message(format!(
                        "Downloaded {} samples.",
                        HumanCount(downloaded_lines as u64)
                    ));
                }

                // now we downloaded the file, rewrite it so we only have the exact sample size
                // and truncate potentially incomplete lines. We streamed the download
                // and the downloaded file may be more than the sample size, and the final
                // line may be incomplete
                let retrieved_name = file.path().to_str().unwrap().to_string();
                let config = Config::new(&Some(retrieved_name))
                    .delimiter(args.flag_delimiter)
                    // we say no_headers so we can just copy the downloaded file over
                    // including headers, to the exact sanple size file
                    .no_headers(true)
                    .flexible(true);

                let mut rdr = config.reader()?;
                let wtr_file = NamedTempFile::new()?;

                // keep the temporary file around so we can sniff it later
                // we'll delete it when we're done
                let (_file, path) = wtr_file
                    .keep()
                    .or(Err("Cannot keep temporary file".to_string()))?;
                let wtr_file_path = path.to_str().unwrap().to_string();

                let mut wtr = Config::new(&Some(wtr_file_path.clone()))
                    .no_headers(false)
                    .flexible(true)
                    .quote_style(csv::QuoteStyle::NonNumeric)
                    .writer()?;
                let mut downloaded_records = 0_usize;

                // amortize allocation
                #[allow(unused_assignments)]
                let mut record = csv::ByteRecord::with_capacity(100, 20);

                let header_row = rdr.byte_headers()?;
                wtr.write_byte_record(header_row)?;
                rdr.byte_records().next();

                for rec in rdr.byte_records() {
                    record = rec?;
                    if downloaded_records >= lines_sample_size {
                        break;
                    }
                    downloaded_records += 1;
                    wtr.write_byte_record(&record)?;
                }
                wtr.flush()?;

                Ok(SniffFileStruct {
                    display_path: url,
                    file_to_sniff: wtr_file_path,
                    tempfile_flag: true,
                    retrieved_size: downloaded,
                    file_size: if total_size == usize::MAX {
                        // the server didn't give us content length, so we just
                        // downloaded the entire file. downloaded variable
                        // is the total size of the file
                        downloaded
                    } else {
                        total_size
                    },
                    downloaded_records,
                })
            }
            // its a file, passthrough the path along with its size
            path => {
                let metadata = fs::metadata(&path)
                    .map_err(|_| format!("Cannot get metadata for file '{path}'"))?;

                let fsize = metadata.len() as usize;

                let canonical_path = fs::canonicalize(&path)?.to_str().unwrap().to_string();

                Ok(SniffFileStruct {
                    display_path:       canonical_path,
                    file_to_sniff:      path,
                    tempfile_flag:      false,
                    retrieved_size:     fsize,
                    file_size:          fsize,
                    downloaded_records: 0,
                })
            }
        }
    } else {
        // read from stdin and write to a temp file
        let mut stdin_file = NamedTempFile::new()?;
        let stdin = std::io::stdin();
        let mut stdin_handle = stdin.lock();
        std::io::copy(&mut stdin_handle, &mut stdin_file)?;
        drop(stdin_handle);
        let (file, path) = stdin_file
            .keep()
            .or(Err("Cannot keep temporary file".to_string()))?;

        let metadata = file
            .metadata()
            .map_err(|_| "Cannot get metadata for stdin file".to_string())?;

        let fsize = metadata.len() as usize;
        let path_string = path
            .into_os_string()
            .into_string()
            .unwrap_or_else(|_| "???".to_string());

        Ok(SniffFileStruct {
            display_path:       "stdin".to_string(),
            file_to_sniff:      path_string,
            tempfile_flag:      true,
            retrieved_size:     fsize,
            file_size:          fsize,
            downloaded_records: 0,
        })
    }
}

fn cleanup_tempfile(
    tempfile_flag: bool,
    tempfile: String,
) -> Result<(), crate::clitypes::CliError> {
    if tempfile_flag {
        fs::remove_file(tempfile)?;
    }
    Ok(())
}

#[allow(clippy::unused_async)] // false positive lint
pub async fn run(argv: &[&str]) -> CliResult<()> {
    let args: Args = util::get_args(USAGE, argv)?;

    let mut sample_size = args.flag_sample;
    if sample_size < 0.0 {
        if args.flag_json || args.flag_pretty_json {
            let json_result = json!({
                "errors": [{
                    "title": "sniff error",
                    "detail": "Sample size must be greater than or equal to zero."
                }]
            });
            return fail_clierror!("{json_result}");
        }
        return fail_clierror!("Sample size must be greater than or equal to zero.");
    }

    let sniffed_ts = chrono::Utc::now().to_rfc3339();

    let future = get_file_to_sniff(&args);
    let sfile_info = block_on(future)?;
    let tempfile_to_delete = sfile_info.file_to_sniff.clone();

    let conf = Config::new(&Some(sfile_info.file_to_sniff.clone()))
        .flexible(true)
        .delimiter(args.flag_delimiter);
    let n_rows = if sfile_info.downloaded_records == 0 {
        match util::count_rows(&conf) {
            Ok(n) => n as usize,
            Err(e) => {
                cleanup_tempfile(sfile_info.tempfile_flag, tempfile_to_delete)?;

                if args.flag_json || args.flag_pretty_json {
                    let json_result = json!({
                        "errors": [{
                            "title": "count rows error",
                            "detail": e.to_string()
                        }]
                    });
                    return fail_clierror!("{json_result}");
                }
                return fail_clierror!("{}", e);
            }
        }
    } else {
        // sfile_info.sampled_records
        // usize::MAX is a sentinel value to let us
        // know that we need to estimate the number of records
        // since we only downloaded a sample,not the entire file
        usize::MAX
    };

    // its an empty file, exit with an error
    if n_rows == 0 {
        cleanup_tempfile(sfile_info.tempfile_flag, tempfile_to_delete)?;

        if args.flag_json || args.flag_pretty_json {
            let json_result = json!({
                "errors": [{
                    "title": "sniff error",
                    "detail": "Empty file"
                }]
            });
            return fail_clierror!("{json_result}");
        }
        return fail_clierror!("Empty file");
    }

    let mut sample_all = false;
    // its a percentage, get the actual sample size
    #[allow(clippy::cast_precision_loss)]
    if sample_size < 1.0 {
        sample_size *= n_rows as f64;
    } else if (sample_size).abs() < f64::EPSILON {
        // its zero, the epsilon bit is because comparing a float
        // is really not precise - see https://floating-point-gui.de/errors/comparison/
        sample_all = true;
    }

    // for a local file and stdin, set sampled_records to the sample size
    // for a remote file, set sampled_records to the number of rows downloaded
    let sampled_records = if sfile_info.downloaded_records == 0 {
        sample_size as usize
    } else {
        sample_all = true;
        sfile_info.downloaded_records
    };

    let rdr = conf.reader_file()?;

    let dt_preference = if args.flag_prefer_dmy || conf.get_dmy_preference() {
        DatePreference::DmyFormat
    } else {
        DatePreference::MdyFormat
    };

    if let Some(save_urlsample) = args.flag_save_urlsample {
        fs::copy(sfile_info.file_to_sniff.clone(), save_urlsample)?;
    }

    let sniff_results = if sample_all {
        log::info!("Sniffing ALL rows...");
        if let Some(delimiter) = args.flag_delimiter {
            Sniffer::new()
                .sample_size(SampleSize::All)
                .date_preference(dt_preference)
                .delimiter(delimiter.as_byte())
                .sniff_reader(rdr.into_inner())
        } else {
            Sniffer::new()
                .sample_size(SampleSize::All)
                .date_preference(dt_preference)
                .sniff_reader(rdr.into_inner())
        }
    } else {
        let mut sniff_size = sample_size as usize;
        // sample_size is at least 20
        if sniff_size < 20 {
            sniff_size = 20;
        }
        log::info!("Sniffing {sniff_size} rows...");
        if let Some(delimiter) = args.flag_delimiter {
            Sniffer::new()
                .sample_size(SampleSize::Records(sniff_size))
                .date_preference(dt_preference)
                .delimiter(delimiter.as_byte())
                .sniff_reader(rdr.into_inner())
        } else {
            Sniffer::new()
                .sample_size(SampleSize::Records(sniff_size))
                .date_preference(dt_preference)
                .sniff_reader(rdr.into_inner())
        }
    };

    let mut processed_results = SniffStruct::default();
    let mut sniffing_error: Option<String> = None;

    match sniff_results {
        Ok(metadata) => {
            let (num_records, estimated) = rowcount(&metadata, &sfile_info, n_rows);

            let sniffedfields = metadata
                .fields
                .iter()
                .map(std::string::ToString::to_string)
                .collect();
            let sniffedtypes = metadata
                .types
                .iter()
                .map(std::string::ToString::to_string)
                .collect();

            processed_results = SniffStruct {
                path: sfile_info.display_path,
                sniff_timestamp: sniffed_ts,
                delimiter_char: metadata.dialect.delimiter as char,
                header_row: metadata.dialect.header.has_header_row,
                preamble_rows: metadata.dialect.header.num_preamble_rows,
                quote_char: match metadata.dialect.quote {
                    qsv_sniffer::metadata::Quote::Some(chr) => format!("{}", char::from(chr)),
                    qsv_sniffer::metadata::Quote::None => "none".into(),
                },
                flexible: metadata.dialect.flexible,
                is_utf8: metadata.dialect.is_utf8,
                retrieved_size: sfile_info.retrieved_size,
                file_size: sfile_info.file_size, // sfile_info.file_size,
                sampled_records: if sampled_records > num_records {
                    num_records
                } else {
                    sampled_records
                },
                estimated,
                num_records,
                avg_record_len: metadata.avg_record_len,
                num_fields: metadata.num_fields,
                fields: sniffedfields,
                types: sniffedtypes,
            };
        }
        Err(e) => {
            sniffing_error = Some(e.to_string());
        }
    }

    cleanup_tempfile(sfile_info.tempfile_flag, tempfile_to_delete)?;

    if args.flag_json || args.flag_pretty_json {
        if sniffing_error.is_none() {
            if args.flag_pretty_json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&processed_results).unwrap()
                );
            } else {
                println!("{}", serde_json::to_string(&processed_results).unwrap());
            };
            Ok(())
        } else {
            let json_error = json!({
                "errors": [{
                    "title": "sniff error",
                    "detail": sniffing_error.unwrap()
                }]
            });
            fail_clierror!("{json_error}")
        }
    } else if sniffing_error.is_none() {
        println!("{processed_results}");
        return Ok(());
    } else {
        return fail_clierror!("{}", sniffing_error.unwrap());
    }
}
