//! Execution profiles for profile-guided optimization (PGO).
//!
//! A program built by sijoc and run with `--profile FILE` writes, at exit, how long each system
//! took and over how many entity rows (measured on one thread), how long each stage's deferred
//! commands took, and what opening a parallel region costs on that machine. `sijoc build
//! --profile-use FILE` reads it back: the planner then knows the cost of an entity row per system
//! instead of guessing. Every decision it changes only affects speed (where the parallel threshold
//! lies, which systems of a stage run side by side, which fields share a component): results stay
//! bit-identical with and without a profile, and a profile from another version of the program is
//! used for the systems that still exist (matched by name) and ignored otherwise.

#[derive(Clone, Debug, Default)]
pub struct SysProfile {
    pub name: String,
    /// times the system ran
    pub calls: u64,
    /// entity rows it ran over, summed over the calls
    pub rows: u64,
    /// time spent, nanoseconds (on one thread)
    pub ns: u64,
}

#[derive(Clone, Debug, Default)]
pub struct Profile {
    /// hash of the system names of the program that wrote it
    pub signature: u64,
    pub ticks: u64,
    /// cost of opening and closing a parallel region with the default thread count, nanoseconds
    pub fork_ns: u64,
    /// default thread count of the machine that wrote it
    pub threads: u32,
    pub systems: Vec<SysProfile>,
    /// per stage: time applying deferred commands, nanoseconds
    pub apply_ns: Vec<u64>,
    /// `--profile-fields` builds: accesses of each entity field over the run (`Class.Field`)
    pub fields: Vec<(String, u64)>,
}

/// a field's name in a profile: `Class.Field` without spaces (`Pool<int,float>.Items`)
pub fn field_key(label: &str) -> String {
    label.replace(' ', "")
}

pub const HEADER: &str = "SIJO-PROFILE 1";

/// FNV-1a over the system names in order (identifies the program shape a profile came from)
pub fn signature<'a>(names: impl Iterator<Item = &'a str>) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for n in names {
        for b in n.bytes().chain(std::iter::once(0u8)) {
            h ^= b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
    }
    h
}

impl Profile {
    pub fn parse(text: &str) -> Result<Profile, String> {
        let mut lines = text.lines();
        if lines.next().map(|l| l.trim()) != Some(HEADER) {
            return Err(format!("not a sijo profile (the first line must be '{}')", HEADER));
        }
        let mut p = Profile::default();
        for (no, l) in lines.enumerate() {
            let t: Vec<&str> = l.split_whitespace().collect();
            let num = |i: usize| -> Result<u64, String> { t.get(i).and_then(|x| x.parse::<u64>().ok()).ok_or_else(|| format!("line {}: expected a number: {}", no + 2, l)) };
            match t.first().copied() {
                None => {}
                Some("signature") => p.signature = u64::from_str_radix(t.get(1).copied().unwrap_or(""), 16).map_err(|_| format!("line {}: bad signature", no + 2))?,
                Some("ticks") => p.ticks = num(1)?,
                Some("fork_ns") => p.fork_ns = num(1)?,
                Some("threads") => p.threads = num(1)? as u32,
                // system <name> calls <n> rows <n> ns <n>
                Some("system") if t.len() == 8 && t[2] == "calls" && t[4] == "rows" && t[6] == "ns" => {
                    p.systems.push(SysProfile { name: t[1].to_string(), calls: num(3)?, rows: num(5)?, ns: num(7)? });
                }
                // field <Class.Field> count <n>
                Some("field") if t.len() == 4 && t[2] == "count" => p.fields.push((t[1].to_string(), num(3)?)),
                Some("apply") if t.len() == 4 && t[2] == "ns" => {
                    let s = num(1)? as usize;
                    if s > 4096 {
                        return Err(format!("line {}: stage number too large", no + 2));
                    }
                    if p.apply_ns.len() <= s {
                        p.apply_ns.resize(s + 1, 0);
                    }
                    p.apply_ns[s] = num(3)?;
                }
                Some(_) => return Err(format!("line {}: unknown entry: {}", no + 2, l)),
            }
        }
        Ok(p)
    }

    pub fn read(path: &std::path::Path) -> Result<Profile, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("cannot read profile {}: {}", path.display(), e))?;
        Profile::parse(&text)
    }

    /// accesses of a field (`Class.Field`), when the profile counted fields
    pub fn field_count(&self, label: &str) -> Option<u64> {
        let k = field_key(label);
        self.fields.iter().find(|(n, _)| *n == k).map(|(_, c)| *c)
    }

    pub fn system(&self, name: &str) -> Option<&SysProfile> {
        self.systems.iter().find(|s| s.name == name)
    }

    /// nanoseconds per entity row (None: never ran over a row)
    pub fn ns_per_row(&self, name: &str) -> Option<f64> {
        self.system(name).filter(|s| s.rows > 0).map(|s| s.ns as f64 / s.rows as f64)
    }

    /// nanoseconds per call
    pub fn ns_per_call(&self, name: &str) -> Option<f64> {
        self.system(name).filter(|s| s.calls > 0).map(|s| s.ns as f64 / s.calls as f64)
    }

    /// share of the measured tick time spent in this system (0..1)
    pub fn share(&self, name: &str) -> f64 {
        let total: u64 = self.systems.iter().map(|s| s.ns).sum::<u64>() + self.apply_ns.iter().sum::<u64>();
        match self.system(name) {
            Some(s) if total > 0 => s.ns as f64 / total as f64,
            _ => 0.0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_what_programs_write() {
        let text = "SIJO-PROFILE 1\nsignature 00ab\nticks 60\nthreads 4\nfork_ns 2500\nsystem Ship.Update calls 60 rows 6000 ns 1200000\nsystem Game.Update calls 60 rows 60 ns 6000\napply 0 ns 300\napply 1 ns 50\n";
        let p = Profile::parse(text).unwrap();
        assert_eq!(p.signature, 0xab);
        assert_eq!(p.fork_ns, 2500);
        assert_eq!(p.ns_per_row("Ship.Update"), Some(200.0));
        assert_eq!(p.ns_per_call("Game.Update"), Some(100.0));
        assert!(p.share("Ship.Update") > 0.99);
        let q = Profile::parse("SIJO-PROFILE 1\nfield Ship.Pos count 900\nfield Pool<int,float>.Items count 3\n").unwrap();
        assert_eq!(q.field_count("Ship.Pos"), Some(900));
        assert_eq!(q.field_count("Pool<int, float>.Items"), Some(3));
        assert_eq!(q.field_count("Ship.Hp"), None);
        assert!(Profile::parse("garbage").is_err());
        assert!(Profile::parse("SIJO-PROFILE 1\nsystem X calls a rows 1 ns 1\n").is_err());
    }
}
