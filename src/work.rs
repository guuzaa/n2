//! Build runner, choosing and executing tasks as determined by out of date inputs.

use crate::db;
use crate::depfile;
use crate::graph::*;
use crate::scanner::Scanner;
use std::collections::{HashMap, HashSet};
use std::io::Write;

pub struct Work<'a> {
    graph: &'a mut Graph,
    db: &'a mut db::Writer,
    files: HashMap<FileId, bool>,
    want: HashSet<BuildId>,
    ready: HashSet<BuildId>,
}

impl<'a> Work<'a> {
    pub fn new(graph: &'a mut Graph, db: &'a mut db::Writer) -> Self {
        Work {
            graph: graph,
            files: HashMap::new(),
            want: HashSet::new(),
            ready: HashSet::new(),
            db: db,
        }
    }

    fn want_build(
        &mut self,
        state: &mut State,
        last_state: &State,
        id: BuildId,
    ) -> Result<bool, String> {
        if self.want.contains(&id) {
            return Ok(true);
        }

        // Visit inputs first, to discover if any are out of date.
        let mut input_dirty = false;
        let ins = self.graph.build(id).depend_ins().to_vec();
        for id in ins {
            let d = self.want_file(state, last_state, id)?;
            input_dirty = input_dirty || d;
        }

        let dirty = input_dirty
            || true /*match last_state.get_hash(id) {
                None => true,
                Some(hash) => hash != state.hash(self.graph, id)?,
            }*/;

        if dirty {
            self.want.insert(id);
            if !input_dirty {
                self.ready.insert(id);
            }
        }

        Ok(dirty)
    }

    pub fn want_file(
        &mut self,
        state: &mut State,
        last_state: &State,
        id: FileId,
    ) -> Result<bool, String> {
        if let Some(dirty) = self.files.get(&id) {
            return Ok(*dirty);
        }

        let dirty = match self.graph.file(id).input {
            None => {
                self.stat(state, id)?;
                state.file_mut(id).hash = Some(Hash::todo()); // ready
                false
            }
            Some(bid) => {
                if self.want_build(state, last_state, bid)? {
                    true
                } else {
                    match self.stat(state, id)? {
                        MTime::Missing => true,
                        MTime::Stamp(_) => {
                            // compare hash
                            false
                        }
                    }
                }
            }
        };

        self.files.insert(id, dirty);
        Ok(dirty)
    }

    pub fn stat(&self, state: &mut State, id: FileId) -> Result<MTime, String> {
        state
            .stat(self.graph, id)
            .map_err(|err| format!("stat {}: {}", self.graph.file(id).name, err))
    }

    fn recheck_ready(&mut self, state: &State, id: BuildId) -> bool {
        let build = self.graph.build(id);
        println!("  recheck {:?} {}", id, build.location);
        for &id in build.depend_ins() {
            let file = self.graph.file(id);
            if state.file(id).hash.is_none() {
                println!("    {:?} {} not ready", id, file.name);
                return false;
            }
        }
        println!("    now ready");
        true
    }

    fn read_depfile(&mut self, id: BuildId, path: &str) -> Result<(), String> {
        let mut bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) => return Err(format!("read {}: {}", path, e)),
        };
        bytes.push(0);

        let mut scanner = Scanner::new(unsafe { std::str::from_utf8_unchecked(&bytes) });
        let parsed_deps = depfile::parse(&mut scanner)
            .map_err(|err| format!("in {}: {}", path, scanner.format_parse_error(err)))?;
        // TODO verify deps refers to correct output
        let previous_deps = self.graph.build_mut(id).take_deps_ins();
        let mut deps: Vec<FileId> = Vec::new();
        for dep in parsed_deps.deps {
            let depid = self.graph.file_id(dep);
            if !self.graph.build(id).depend_ins().contains(&depid) {
                deps.push(depid);
            }
        }
        if deps != previous_deps {
            println!("deps change: {:?} => {:?}", previous_deps, deps,);
            println!(
                "deps change: {:?} => {:?}",
                previous_deps
                    .iter()
                    .map(|&id| &self.graph.file(id).name)
                    .collect::<Vec<_>>(),
                deps.iter()
                    .map(|&id| &self.graph.file(id).name)
                    .collect::<Vec<_>>(),
            );
            self.db
                .write_deps(self.graph, &self.graph.build(id).outs(), &deps)
                .map_err(|err| err.to_string())?;
        }
        Ok(())
    }

    fn run_one(&mut self, id: BuildId) -> Result<(), String> {
        let build = self.graph.build(id);
        let cmdline = match &build.cmdline {
            None => return Ok(()),
            Some(c) => c,
        };
        println!("$ {}", cmdline);
        let output = std::process::Command::new("sh")
            .arg("-c")
            .arg(cmdline)
            .output()
            .map_err(|err| format!("{}", err))?;
        if !output.stdout.is_empty() {
            std::io::stdout()
                .write_all(&output.stdout)
                .map_err(|err| format!("{}", err))?;
        }
        if !output.stderr.is_empty() {
            std::io::stdout()
                .write_all(&output.stderr)
                .map_err(|err| format!("{}", err))?;
        }
        if !output.status.success() {
            return Err(format!("subcommand failed"));
        }
        if let Some(depfile) = &build.depfile {
            let depfile = &depfile.clone();
            self.read_depfile(id, depfile)?;
        }
        Ok(())
    }

    fn build_finished(&mut self, state: &mut State, id: BuildId) {
        let build = self.graph.build(id);
        println!("finished {:?} {}", id, build.location);
        let hash = state.hash(self.graph, id);
        let mut ready_files = HashSet::new();
        for &id in build.outs() {
            let file = self.graph.file(id);
            println!("  wrote {:?} {:?}", id, file.name);
            state.file_mut(id).mtime = Some(MTime::Stamp(1));
            state.file_mut(id).hash = Some(hash);
            for &id in &file.dependents {
                if !self.want.contains(&id) {
                    continue;
                }
                ready_files.insert(id);
            }
        }
        for id in ready_files {
            if !self.recheck_ready(state, id) {
                continue;
            }
            self.ready.insert(id);
        }
    }

    pub fn run(&mut self, state: &mut State) -> Result<(), String> {
        while !self.want.is_empty() {
            let id = match self.ready.iter().next() {
                None => {
                    panic!("no ready, but want {:?}", self.want);
                }
                Some(&id) => id,
            };
            self.want.remove(&id);
            self.ready.remove(&id);
            self.run_one(id)?;
            self.build_finished(state, id);
        }
        Ok(())
    }
}