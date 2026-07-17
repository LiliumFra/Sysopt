use crate::{Action, Priority, ProcessPowerPolicy, TimerResolutionPolicy};
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
struct ManagedPriority {
    start_time: u64,
    name: String,
    priority: Priority,
}

#[derive(Debug, Clone)]
struct ManagedPowerPolicy {
    start_time: u64,
    name: String,
    power: ProcessPowerPolicy,
    timer_resolution: TimerResolutionPolicy,
}

#[derive(Debug, Clone)]
struct ManagedGroup {
    start_time: u64,
    name: String,
    group: crate::ResourceGroup,
    limits: crate::ResourceLimits,
    changed_at: Instant,
}

/// Rastrea únicamente cambios confirmados por el sistema operativo y evita
/// actuar sobre un PID reciclado.
pub struct PriorityTracker {
    priorities: HashMap<u32, ManagedPriority>,
    power_policies: HashMap<u32, ManagedPowerPolicy>,
    groups: HashMap<u32, ManagedGroup>,
    group_min_residency: Duration,
}

impl Default for PriorityTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl PriorityTracker {
    pub fn new() -> Self {
        Self::with_group_min_residency(Duration::from_secs(30))
    }

    pub fn with_group_min_residency(group_min_residency: Duration) -> Self {
        Self {
            priorities: HashMap::new(),
            power_policies: HashMap::new(),
            groups: HashMap::new(),
            group_min_residency,
        }
    }

    /// Construye el plan del ciclo sin asumir que las acciones se aplicarán.
    pub fn plan(
        &mut self,
        mut actions: Vec<Action>,
        is_same_process: impl Fn(u32, u64) -> bool + Copy,
    ) -> Vec<Action> {
        self.prune_stale(is_same_process);

        // Un PID puede reciclarse entre la captura y la aplicación. Se descarta
        // cualquier acción cuya identidad (PID + start_time) ya no coincida.
        actions.retain(|action| is_same_process(action.pid(), action.start_time()));

        // Varias capas (reglas, aprendizaje e IA) pueden proponer la misma clase
        // de acción para un PID. Conservamos la última decisión de cada clase para
        // no ejecutar syscalls contradictorias dentro del mismo ciclo.
        let mut seen_priorities = HashSet::new();
        let mut seen_power = HashSet::new();
        let mut seen_groups = HashSet::new();
        actions.reverse();
        actions.retain(|action| match action {
            Action::SetProcessPriority { pid, .. } => seen_priorities.insert(*pid),
            Action::SetProcessPowerPolicy { pid, .. } => seen_power.insert(*pid),
            Action::AssignProcessGroup { pid, .. } | Action::ResetProcessGroup { pid, .. } => {
                seen_groups.insert(*pid)
            }
        });
        actions.reverse();

        // Se calculan sobre las solicitudes originales. Una migración de cgroup
        // suprimida temporalmente sigue contando como gestionada y no provoca un
        // reset accidental en el mismo ciclo.
        let priority_acted: HashSet<u32> = actions
            .iter()
            .filter_map(|action| match action {
                Action::SetProcessPriority { pid, .. } => Some(*pid),
                _ => None,
            })
            .collect();
        let power_acted: HashSet<u32> = actions
            .iter()
            .filter_map(|action| match action {
                Action::SetProcessPowerPolicy { pid, .. } => Some(*pid),
                _ => None,
            })
            .collect();
        let group_acted: HashSet<u32> = actions
            .iter()
            .filter_map(|action| match action {
                Action::AssignProcessGroup { pid, .. } | Action::ResetProcessGroup { pid, .. } => {
                    Some(*pid)
                }
                _ => None,
            })
            .collect();

        let now = Instant::now();
        actions.retain(|action| match action {
            Action::SetProcessPriority {
                pid,
                start_time,
                priority,
                ..
            } => match self.priorities.get(pid) {
                Some(managed) if managed.start_time == *start_time => managed.priority != *priority,
                None => *priority != Priority::Normal,
                Some(_) => true,
            },
            Action::SetProcessPowerPolicy {
                pid,
                start_time,
                power,
                timer_resolution,
                ..
            } => match self.power_policies.get(pid) {
                Some(managed) if managed.start_time == *start_time => {
                    managed.power != *power || managed.timer_resolution != *timer_resolution
                }
                None => {
                    *power != ProcessPowerPolicy::SystemManaged
                        || *timer_resolution != TimerResolutionPolicy::SystemManaged
                }
                Some(_) => true,
            },
            Action::AssignProcessGroup {
                pid,
                start_time,
                group,
                limits,
                ..
            } => match self.groups.get(pid) {
                Some(managed) if managed.start_time == *start_time => {
                    if managed.group == *group && managed.limits == *limits {
                        false
                    } else {
                        !(managed.group != *group
                            && now.duration_since(managed.changed_at) < self.group_min_residency)
                    }
                }
                _ => true,
            },
            Action::ResetProcessGroup {
                pid, start_time, ..
            } => self
                .groups
                .get(pid)
                .is_some_and(|managed| managed.start_time == *start_time),
        });

        let mut result = actions;

        for (&pid, managed) in &self.priorities {
            if !priority_acted.contains(&pid)
                && managed.priority != Priority::Normal
                && is_same_process(pid, managed.start_time)
            {
                result.push(Action::SetProcessPriority {
                    pid,
                    start_time: managed.start_time,
                    name: managed.name.clone(),
                    priority: Priority::Normal,
                });
            }
        }

        for (&pid, managed) in &self.power_policies {
            if !power_acted.contains(&pid)
                && is_same_process(pid, managed.start_time)
                && (managed.power != ProcessPowerPolicy::SystemManaged
                    || managed.timer_resolution != TimerResolutionPolicy::SystemManaged)
            {
                result.push(Action::SetProcessPowerPolicy {
                    pid,
                    start_time: managed.start_time,
                    name: managed.name.clone(),
                    power: ProcessPowerPolicy::SystemManaged,
                    timer_resolution: TimerResolutionPolicy::SystemManaged,
                });
            }
        }

        for (&pid, managed) in &self.groups {
            if !group_acted.contains(&pid) && is_same_process(pid, managed.start_time) {
                result.push(Action::ResetProcessGroup {
                    pid,
                    start_time: managed.start_time,
                    name: managed.name.clone(),
                });
            }
        }

        result
    }

    /// Confirma únicamente las acciones reportadas como exitosas por el
    /// enforcer. Los fallos permanecen administrados para reintento o limpieza.
    pub fn commit(
        &mut self,
        applied: &[Action],
        is_same_process: impl Fn(u32, u64) -> bool + Copy,
    ) {
        self.prune_stale(is_same_process);
        for action in applied {
            if !is_same_process(action.pid(), action.start_time()) {
                continue;
            }
            match action {
                Action::SetProcessPriority {
                    pid,
                    start_time,
                    name,
                    priority,
                } => {
                    if *priority == Priority::Normal {
                        self.priorities.remove(pid);
                    } else {
                        self.priorities.insert(
                            *pid,
                            ManagedPriority {
                                start_time: *start_time,
                                name: name.clone(),
                                priority: *priority,
                            },
                        );
                    }
                }
                Action::SetProcessPowerPolicy {
                    pid,
                    start_time,
                    name,
                    power,
                    timer_resolution,
                } => {
                    if *power == ProcessPowerPolicy::SystemManaged
                        && *timer_resolution == TimerResolutionPolicy::SystemManaged
                    {
                        self.power_policies.remove(pid);
                    } else {
                        self.power_policies.insert(
                            *pid,
                            ManagedPowerPolicy {
                                start_time: *start_time,
                                name: name.clone(),
                                power: *power,
                                timer_resolution: *timer_resolution,
                            },
                        );
                    }
                }
                Action::AssignProcessGroup {
                    pid,
                    start_time,
                    name,
                    group,
                    limits,
                } => {
                    let changed_at = self
                        .groups
                        .get(pid)
                        .filter(|managed| {
                            managed.start_time == *start_time && managed.group == *group
                        })
                        .map_or_else(Instant::now, |managed| managed.changed_at);
                    self.groups.insert(
                        *pid,
                        ManagedGroup {
                            start_time: *start_time,
                            name: name.clone(),
                            group: *group,
                            limits: *limits,
                            changed_at,
                        },
                    );
                }
                Action::ResetProcessGroup { pid, .. } => {
                    self.groups.remove(pid);
                }
            }
        }
    }

    pub fn reset_all(&mut self, is_same_process: impl Fn(u32, u64) -> bool + Copy) -> Vec<Action> {
        self.prune_stale(is_same_process);
        let mut actions = Vec::with_capacity(
            self.priorities.len() + self.power_policies.len() + self.groups.len(),
        );
        actions.extend(
            self.priorities
                .iter()
                .filter(|(pid, managed)| is_same_process(**pid, managed.start_time))
                .map(|(&pid, managed)| Action::SetProcessPriority {
                    pid,
                    start_time: managed.start_time,
                    name: managed.name.clone(),
                    priority: Priority::Normal,
                }),
        );
        actions.extend(
            self.power_policies
                .iter()
                .filter(|(pid, managed)| is_same_process(**pid, managed.start_time))
                .map(|(&pid, managed)| Action::SetProcessPowerPolicy {
                    pid,
                    start_time: managed.start_time,
                    name: managed.name.clone(),
                    power: ProcessPowerPolicy::SystemManaged,
                    timer_resolution: TimerResolutionPolicy::SystemManaged,
                }),
        );
        actions.extend(
            self.groups
                .iter()
                .filter(|(pid, managed)| is_same_process(**pid, managed.start_time))
                .map(|(&pid, managed)| Action::ResetProcessGroup {
                    pid,
                    start_time: managed.start_time,
                    name: managed.name.clone(),
                }),
        );
        actions
    }

    pub fn is_empty(&self) -> bool {
        self.priorities.is_empty() && self.power_policies.is_empty() && self.groups.is_empty()
    }

    pub fn len(&self) -> usize {
        self.priorities.len() + self.power_policies.len() + self.groups.len()
    }

    fn prune_stale(&mut self, is_same_process: impl Fn(u32, u64) -> bool + Copy) {
        self.priorities
            .retain(|&pid, managed| is_same_process(pid, managed.start_time));
        self.power_policies
            .retain(|&pid, managed| is_same_process(pid, managed.start_time));
        self.groups
            .retain(|&pid, managed| is_same_process(pid, managed.start_time));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ResourceGroup, ResourceLimits};

    fn priority(pid: u32, start_time: u64, level: Priority) -> Action {
        Action::SetProcessPriority {
            pid,
            start_time,
            name: format!("process-{pid}"),
            priority: level,
        }
    }

    #[test]
    fn solo_confirma_acciones_exitosas() {
        let mut tracker = PriorityTracker::new();
        let planned = tracker.plan(vec![priority(1, 10, Priority::High)], |_, _| true);
        assert_eq!(planned.len(), 1);
        assert!(tracker.is_empty());
        tracker.commit(&planned, |_, _| true);
        assert_eq!(tracker.len(), 1);
    }

    #[test]
    fn restaura_prioridad_y_grupo() {
        let mut tracker = PriorityTracker::new();
        let applied = vec![
            priority(1, 10, Priority::AboveNormal),
            Action::AssignProcessGroup {
                pid: 1,
                start_time: 10,
                name: "rustc".into(),
                group: ResourceGroup::Development,
                limits: ResourceLimits {
                    cpu_weight: Some(300),
                    io_weight: Some(300),
                    memory_high_bytes: None,
                },
            },
        ];
        tracker.commit(&applied, |pid, start| pid == 1 && start == 10);

        let actions = tracker.plan(Vec::new(), |pid, start| pid == 1 && start == 10);
        assert_eq!(actions.len(), 2);
        assert!(actions.iter().any(|action| matches!(
            action,
            Action::SetProcessPriority {
                priority: Priority::Normal,
                ..
            }
        )));
        assert!(actions
            .iter()
            .any(|action| matches!(action, Action::ResetProcessGroup { .. })));
        assert_eq!(tracker.len(), 2);
        tracker.commit(&actions, |pid, start| pid == 1 && start == 10);
        assert!(tracker.is_empty());
    }

    #[test]
    fn no_repite_acciones_ya_confirmadas() {
        let mut tracker = PriorityTracker::with_group_min_residency(Duration::ZERO);
        let group = Action::AssignProcessGroup {
            pid: 9,
            start_time: 90,
            name: "cargo".into(),
            group: ResourceGroup::Development,
            limits: ResourceLimits {
                cpu_weight: Some(300),
                io_weight: Some(250),
                memory_high_bytes: None,
            },
        };
        let applied = vec![priority(9, 90, Priority::AboveNormal), group.clone()];
        tracker.commit(&applied, |pid, start| pid == 9 && start == 90);

        let planned = tracker.plan(applied, |pid, start| pid == 9 && start == 90);
        assert!(planned.is_empty());
    }

    #[test]
    fn conserva_solo_la_ultima_decision_por_clase() {
        let mut tracker = PriorityTracker::with_group_min_residency(Duration::ZERO);
        let actions = tracker.plan(
            vec![
                priority(4, 40, Priority::BelowNormal),
                priority(4, 40, Priority::High),
            ],
            |pid, start| pid == 4 && start == 40,
        );
        assert_eq!(actions.len(), 1);
        assert!(matches!(
            actions[0],
            Action::SetProcessPriority {
                priority: Priority::High,
                ..
            }
        ));
    }

    #[test]
    fn evita_migraciones_de_grupo_demasiado_frecuentes() {
        let mut tracker = PriorityTracker::with_group_min_residency(Duration::from_secs(600));
        tracker.commit(
            &[Action::AssignProcessGroup {
                pid: 11,
                start_time: 110,
                name: "editor".into(),
                group: ResourceGroup::Development,
                limits: ResourceLimits::default(),
            }],
            |pid, start| pid == 11 && start == 110,
        );

        let actions = tracker.plan(
            vec![Action::AssignProcessGroup {
                pid: 11,
                start_time: 110,
                name: "editor".into(),
                group: ResourceGroup::Foreground,
                limits: ResourceLimits::default(),
            }],
            |pid, start| pid == 11 && start == 110,
        );
        assert!(actions.is_empty());
    }

    #[test]
    fn pid_reciclado_no_se_toca() {
        let mut tracker = PriorityTracker::new();
        tracker.commit(&[priority(1, 10, Priority::High)], |_, _| true);
        let actions = tracker.plan(Vec::new(), |pid, start| pid == 1 && start == 11);
        assert!(actions.is_empty());
        assert!(tracker.is_empty());
    }

    #[test]
    fn reset_final_ignora_pid_reciclado() {
        let mut tracker = PriorityTracker::new();
        tracker.commit(&[priority(1, 10, Priority::High)], |_, _| true);
        let actions = tracker.reset_all(|pid, start| pid == 1 && start == 11);
        assert!(actions.is_empty());
    }

    #[test]
    fn descarta_accion_nueva_para_pid_reciclado() {
        let mut tracker = PriorityTracker::new();
        let actions = tracker.plan(vec![priority(7, 100, Priority::High)], |pid, start| {
            pid == 7 && start == 101
        });
        assert!(actions.is_empty());
        assert!(tracker.is_empty());
    }
}
