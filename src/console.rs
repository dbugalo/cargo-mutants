// Copyright 2021, 2022 Martin Pool

//! Print messages and progress bars on the terminal.

use std::borrow::Cow;
use std::fmt::Write;
use std::fs::File;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ::console::{style, StyledObject};
use camino::Utf8Path;

use tracing::Level;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::prelude::*;

use crate::outcome::{LabOutcome, SummaryOutcome};
use crate::*;

static COPY_MESSAGE: &str = "Copy source to scratch directory";

/// An interface to the console for the rest of cargo-mutants.
///
/// This wraps the Nutmeg view and model.
pub struct Console {
    /// The inner view through which progress bars and messages are drawn.
    view: Arc<nutmeg::View<LabModel>>,

    /// The `mutants.out/debug.log` file, if it's open yet.
    debug_log: Arc<Mutex<Option<File>>>,
}

impl Console {
    pub fn new() -> Console {
        Console {
            view: Arc::new(nutmeg::View::new(LabModel::default(), nutmeg_options())),
            debug_log: Arc::new(Mutex::new(None)),
        }
    }

    /// Update that a cargo task is starting.
    pub fn scenario_started(&self, scenario: &Scenario, log_file: &Utf8Path) {
        let start = Instant::now();
        let scenario_model = ScenarioModel::new(scenario, start, log_file.to_owned());
        self.view.update(|model| {
            model.scenario_models.push(scenario_model);
        });
    }

    /// Update that cargo finished.
    pub fn scenario_finished(
        &self,
        scenario: &Scenario,
        outcome: &ScenarioOutcome,
        options: &Options,
    ) {
        self.view.update(|model| {
            model.mutants_done += scenario.is_mutant() as usize;
            match outcome.summary() {
                SummaryOutcome::CaughtMutant => model.mutants_caught += 1,
                SummaryOutcome::MissedMutant => model.mutants_missed += 1,
                SummaryOutcome::Timeout => model.timeouts += 1,
                SummaryOutcome::Unviable => model.unviable += 1,
                SummaryOutcome::Success => model.successes += 1,
                SummaryOutcome::Failure => model.failures += 1,
            }
            model.remove_scenario(scenario);
        });

        if (outcome.mutant_caught() && !options.print_caught)
            || (outcome.scenario.is_mutant()
                && outcome.check_or_build_failed()
                && !options.print_unviable)
        {
            return;
        }

        let mut s = String::with_capacity(100);
        write!(
            s,
            "{} ... {}",
            style_scenario(scenario),
            style_outcome(outcome)
        )
        .unwrap();
        if options.show_times {
            let prs: Vec<String> = outcome
                .phase_results()
                .iter()
                .map(|pr| {
                    format!(
                        "{secs} {phase}",
                        secs = style_secs(pr.duration),
                        phase = style(pr.phase.to_string()).dim()
                    )
                })
                .collect();
            let _ = write!(s, " in {}", prs.join(" + "));
        }
        if outcome.should_show_logs() || options.show_all_logs {
            s.push('\n');
            s.push_str(
                outcome
                    .get_log_content()
                    .expect("read log content")
                    .as_str(),
            );
        }
        s.push('\n');
        self.view.message(&s);
    }

    /// Update that a test timeout was auto-set.
    pub fn autoset_timeout(&self, timeout: Duration) {
        self.message(&format!(
            "Auto-set test timeout to {}\n",
            style_secs(timeout)
        ));
    }

    pub fn build_dirs_start(&self, _n: usize) {
        // self.message(&format!("Make {n} more build directories...\n"));
    }

    pub fn build_dirs_finished(&self) {}

    // pub fn start_copy(&self) {
    //     self.view.update(|model| {
    //         assert!(model.copy_model.is_none());
    //         model.copy_model = Some(CopyModel::new());
    //     });
    // }

    // pub fn finish_copy(&self) {
    //     self.view.update(|model| {
    //         model.copy_model = None;
    //     });
    // }

    // pub fn copy_progress(&self, total_bytes: u64) {
    //     self.view.update(|model| {
    //         model
    //             .copy_model
    //             .as_mut()
    //             .expect("copy in progress")
    //             .bytes_copied(total_bytes)
    //     });
    // }

    /// Update that we discovered some mutants to test.
    pub fn discovered_mutants(&self, mutants: &[Mutant]) {
        self.message(&format!(
            "Found {} to test\n",
            plural(mutants.len(), "mutant")
        ));
        let n_mutants = mutants.len();
        self.view.update(|model| {
            model.n_mutants = n_mutants;
            model.lab_start_time = Some(Instant::now());
        })
    }

    /// Update that work is starting on testing a given number of mutants.
    pub fn start_testing_mutants(&self, _n_mutants: usize) {
        self.view
            .update(|model| model.mutants_start_time = Some(Instant::now()));
    }

    /// A new phase of this scenario started.
    pub fn scenario_phase_started(&self, scenario: &Scenario, phase: Phase) {
        self.view.update(|model| {
            model.find_scenario_mut(scenario).phase_started(phase);
        })
    }

    pub fn scenario_phase_finished(&self, scenario: &Scenario, phase: Phase) {
        self.view.update(|model| {
            model.find_scenario_mut(scenario).phase_finished(phase);
        })
    }

    pub fn lab_finished(&self, lab_outcome: &LabOutcome, start_time: Instant, options: &Options) {
        self.view.update(|model| {
            model.scenario_models.clear();
        });
        self.message(&format!(
            "{}\n",
            lab_outcome.summary_string(start_time, options)
        ));
    }

    pub fn message(&self, message: &str) {
        self.view.message(message)
    }

    pub fn tick(&self) {
        self.view.update(|_| ())
    }

    /// Return a tracing `MakeWriter` that will send messages via nutmeg to the console.
    pub fn make_terminal_writer(&self) -> TerminalWriter {
        TerminalWriter {
            view: Arc::clone(&self.view),
        }
    }

    /// Return a tracing `MakeWriter` that will send messages to the debug log file if
    /// it's open.
    pub fn make_debug_log_writer(&self) -> DebugLogWriter {
        DebugLogWriter(Arc::clone(&self.debug_log))
    }

    /// Set the debug log file.
    pub fn set_debug_log(&self, file: File) {
        *self.debug_log.lock().unwrap() = Some(file);
    }

    /// Configure tracing to send messages to the console and debug log.
    ///
    /// The debug log is opened later and provided by [Console::set_debug_log].
    pub fn setup_global_trace(&self, console_trace_level: Level) -> Result<()> {
        // Show time relative to the start of the program.
        let uptime = tracing_subscriber::fmt::time::uptime();
        let debug_log_layer = tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .with_file(true) // source file name
            .with_line_number(true)
            .with_timer(uptime)
            .with_writer(self.make_debug_log_writer());
        let level_filter = tracing_subscriber::filter::LevelFilter::from_level(console_trace_level);
        let console_layer = tracing_subscriber::fmt::layer()
            .with_ansi(true)
            .with_writer(self.make_terminal_writer())
            .with_target(false)
            .with_timer(uptime)
            .with_filter(level_filter);
        tracing_subscriber::registry()
            .with(debug_log_layer)
            .with(console_layer)
            .init();
        Ok(())
    }
}

/// Write trace output to the terminal via the console.
pub struct TerminalWriter {
    view: Arc<nutmeg::View<LabModel>>,
}

impl<'w> MakeWriter<'w> for TerminalWriter {
    type Writer = Self;

    fn make_writer(&self) -> Self::Writer {
        TerminalWriter {
            view: Arc::clone(&self.view),
        }
    }
}

impl std::io::Write for TerminalWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        // This calls `message` rather than `View::write` because the latter
        // only requires a &View and it handles locking internally, without
        // requiring exclusive use of the Arc<View>.
        self.view.message(std::str::from_utf8(buf).unwrap());
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Write trace output to the debug log file if it's open.
pub struct DebugLogWriter(Arc<Mutex<Option<File>>>);

impl<'w> MakeWriter<'w> for DebugLogWriter {
    type Writer = Self;

    fn make_writer(&self) -> Self::Writer {
        DebugLogWriter(self.0.clone())
    }
}

impl io::Write for DebugLogWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if let Some(file) = self.0.lock().unwrap().as_mut() {
            file.write(buf)
        } else {
            Ok(buf.len())
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        if let Some(file) = self.0.lock().unwrap().as_mut() {
            file.flush()
        } else {
            Ok(())
        }
    }
}

/// Description of all current activities in the lab.
///
/// At the moment there is either a copy, cargo runs, or nothing.  Later, there
/// might be concurrent activities.
#[derive(Default)]
struct LabModel {
    copy_model: Option<CopyModel>,
    scenario_models: Vec<ScenarioModel>,
    lab_start_time: Option<Instant>,
    // The instant when we started trying mutation scenarios, after running the baseline.
    mutants_start_time: Option<Instant>,
    mutants_done: usize,
    n_mutants: usize,
    mutants_caught: usize,
    mutants_missed: usize,
    unviable: usize,
    timeouts: usize,
    successes: usize,
    failures: usize,
}

impl nutmeg::Model for LabModel {
    fn render(&mut self, width: usize) -> String {
        let mut s = String::with_capacity(100);
        if let Some(copy) = self.copy_model.as_mut() {
            s.push_str(&copy.render(width));
        }
        if !s.is_empty() {
            s.push('\n')
        }
        if let Some(lab_start_time) = self.lab_start_time {
            let elapsed = lab_start_time.elapsed();
            let percent = if self.n_mutants > 0 {
                ((self.mutants_done as f64) / (self.n_mutants as f64) * 100.0).round()
            } else {
                0.0
            };
            write!(
                s,
                "{}/{} mutants tested, {}% done",
                style(self.mutants_done).cyan(),
                style(self.n_mutants).cyan(),
                style(percent).cyan(),
            )
            .unwrap();
            if self.mutants_missed > 0 {
                write!(
                    s,
                    ", {} {}",
                    style(self.mutants_missed).cyan(),
                    style("missed").red()
                )
                .unwrap();
            }
            if self.timeouts > 0 {
                write!(
                    s,
                    ", {} {}",
                    style(self.timeouts).cyan(),
                    style("timeout").red()
                )
                .unwrap();
            }
            if self.mutants_caught > 0 {
                write!(s, ", {} caught", style(self.mutants_caught).cyan()).unwrap();
            }
            if self.unviable > 0 {
                write!(s, ", {} unviable", style(self.unviable).cyan()).unwrap();
            }
            // Maybe don't report these, because they're uninteresting?
            // if self.successes > 0 {
            //     write!(s, ", {} successes", self.successes).unwrap();
            // }
            // if self.failures > 0 {
            //     write!(s, ", {} failures", self.failures).unwrap();
            // }
            write!(s, ", {} elapsed", style_minutes_seconds(elapsed)).unwrap();
            if self.mutants_done > 2 {
                write!(
                    s,
                    ", about {} remaining",
                    style(nutmeg::estimate_remaining(
                        &self.mutants_start_time.unwrap(),
                        self.mutants_done,
                        self.n_mutants
                    ))
                    .cyan()
                )
                .unwrap();
            }
            writeln!(s).unwrap();
        }
        for sm in self.scenario_models.iter_mut() {
            s.push_str(&sm.render(width));
            s.push('\n');
        }
        while s.ends_with('\n') {
            s.pop();
        }
        s
    }
}

impl LabModel {
    fn find_scenario_mut(&mut self, scenario: &Scenario) -> &mut ScenarioModel {
        self.scenario_models
            .iter_mut()
            .find(|sm| sm.scenario == *scenario)
            .expect("scenario is in progress")
    }

    fn remove_scenario(&mut self, scenario: &Scenario) {
        self.scenario_models.retain(|sm| sm.scenario != *scenario);
    }
}

/// A Nutmeg progress model for running a single scenario.
///
/// It draws the command and some description of what scenario is being tested.
struct ScenarioModel {
    scenario: Scenario,
    name: Cow<'static, str>,
    phase_start: Instant,
    phase: Option<Phase>,
    /// Previously-executed phases and durations.
    previous_phase_durations: Vec<(Phase, Duration)>,
    log_file: Utf8PathBuf,
}

impl ScenarioModel {
    fn new(scenario: &Scenario, start: Instant, log_file: Utf8PathBuf) -> ScenarioModel {
        ScenarioModel {
            scenario: scenario.clone(),
            name: style_scenario(scenario),
            phase: None,
            phase_start: start,
            log_file,
            previous_phase_durations: Vec::new(),
        }
    }

    fn phase_started(&mut self, phase: Phase) {
        self.phase = Some(phase);
        self.phase_start = Instant::now();
    }

    fn phase_finished(&mut self, phase: Phase) {
        debug_assert_eq!(self.phase, Some(phase));
        self.previous_phase_durations
            .push((phase, self.phase_start.elapsed()));
        self.phase = None;
    }
}

impl nutmeg::Model for ScenarioModel {
    fn render(&mut self, _width: usize) -> String {
        let mut s = String::with_capacity(100);
        write!(s, "{} ... ", self.name).unwrap();
        let mut prs = self
            .previous_phase_durations
            .iter()
            .map(|(phase, duration)| format!("{} {}", style_secs(*duration), style(phase).dim()))
            .collect::<Vec<_>>();
        if let Some(phase) = self.phase {
            prs.push(format!(
                "{} {}",
                style_secs(self.phase_start.elapsed()),
                style(phase).dim()
            ));
        }
        write!(s, "{}", prs.join(" + ")).unwrap();
        if let Ok(last_line) = last_line(&self.log_file) {
            write!(s, "\n    {}", style(last_line).dim()).unwrap();
        }
        s
    }
}

/// A Nutmeg model for progress in copying a tree.
struct CopyModel {
    bytes_copied: u64,
    start: Instant,
}

impl CopyModel {
    #[allow(dead_code)]
    fn new() -> CopyModel {
        CopyModel {
            start: Instant::now(),
            bytes_copied: 0,
        }
    }

    /// Update that some bytes have been copied.
    ///
    /// `bytes_copied` is the total bytes copied so far.
    #[allow(dead_code)]
    fn bytes_copied(&mut self, bytes_copied: u64) {
        self.bytes_copied = bytes_copied
    }
}

impl nutmeg::Model for CopyModel {
    fn render(&mut self, _width: usize) -> String {
        format!(
            "{} ... {} in {}",
            COPY_MESSAGE,
            style_mb(self.bytes_copied),
            style_elapsed_secs(self.start),
        )
    }
}

fn nutmeg_options() -> nutmeg::Options {
    nutmeg::Options::default().print_holdoff(Duration::from_millis(50))
}

/// Return a styled string reflecting the moral value of this outcome.
pub fn style_outcome(outcome: &ScenarioOutcome) -> StyledObject<&'static str> {
    match outcome.summary() {
        SummaryOutcome::CaughtMutant => style("caught").green(),
        SummaryOutcome::MissedMutant => style("NOT CAUGHT").red().bold(),
        SummaryOutcome::Failure => style("FAILED").red().bold(),
        SummaryOutcome::Success => style("ok").green(),
        SummaryOutcome::Unviable => style("unviable").blue(),
        SummaryOutcome::Timeout => style("TIMEOUT").red().bold(),
    }
}

pub fn list_mutants(mutants: &[Mutant], show_diffs: bool) {
    for mutant in mutants {
        println!("{}", style_mutant(mutant));
        if show_diffs {
            println!("{}", mutant.diff());
        }
    }
}

fn style_mutant(mutant: &Mutant) -> String {
    // This is like `impl Display for Mutant`, but with colors.
    // The text content should be the same.
    format!(
        "{}: replace {}{}{} with {}",
        mutant.describe_location(),
        style(mutant.function_name()).bright().magenta(),
        if mutant.return_type().is_empty() {
            ""
        } else {
            " "
        },
        style(mutant.return_type()).magenta(),
        style(mutant.replacement_text()).yellow(),
    )
}

fn style_elapsed_secs(since: Instant) -> String {
    style_secs(since.elapsed())
}

fn style_secs(duration: Duration) -> String {
    style(format!("{:.1}s", duration.as_secs_f32()))
        .cyan()
        .to_string()
}

fn style_minutes_seconds(duration: Duration) -> String {
    style(duration_minutes_seconds(duration)).cyan().to_string()
}

pub fn duration_minutes_seconds(duration: Duration) -> String {
    let secs = duration.as_secs();
    format!("{}:{:02}", secs / 60, secs % 60)
}

fn format_mb(bytes: u64) -> String {
    format!("{} MB", bytes / 1_000_000)
}

fn style_mb(bytes: u64) -> StyledObject<String> {
    style(format_mb(bytes)).cyan()
}

pub fn style_scenario(scenario: &Scenario) -> Cow<'static, str> {
    match scenario {
        Scenario::Baseline => "Unmutated baseline".into(),
        Scenario::Mutant(mutant) => console::style_mutant(mutant).into(),
    }
}

pub fn plural(n: usize, noun: &str) -> String {
    if n == 1 {
        format!("{n} {noun}")
    } else {
        format!("{n} {noun}s")
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use std::time::Duration;

    #[test]
    fn test_duration_minutes_seconds() {
        assert_eq!(duration_minutes_seconds(Duration::ZERO), "0:00");
        assert_eq!(duration_minutes_seconds(Duration::from_secs(3)), "0:03");
        assert_eq!(duration_minutes_seconds(Duration::from_secs(73)), "1:13");
        assert_eq!(
            duration_minutes_seconds(Duration::from_secs(6003)),
            "100:03"
        );
    }
}
