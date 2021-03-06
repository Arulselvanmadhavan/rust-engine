// Copyright 2014 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Abstraction of a thread pool for basic parallelism.

use std::sync::mpsc::{channel, Sender, Receiver};
use std::sync::{Arc, Mutex};
use std::thread;

trait FnBox {
    fn call_box(self: Box<Self>);
}

impl<F: FnOnce()> FnBox for F {
    fn call_box(self: Box<F>) {
        (*self)()
    }
}

type Thunk<'a> = Box<FnBox + Send + 'a>;

struct Sentinel<'a> {
    jobs: &'a Arc<Mutex<Receiver<Thunk<'static>>>>,
    thread_counter: &'a Arc<Mutex<usize>>,
    thread_count_max: &'a Arc<Mutex<usize>>,
    active: bool,
    request_counter: u64
}

impl<'a> Sentinel<'a> {
    fn new(jobs: &'a Arc<Mutex<Receiver<Thunk<'static>>>>,
           thread_counter: &'a Arc<Mutex<usize>>,
           thread_count_max: &'a Arc<Mutex<usize>>) -> Sentinel<'a> {
        Sentinel {
            jobs: jobs,
            thread_counter: thread_counter,
            thread_count_max: thread_count_max,
            active: true,
            request_counter: 0
        }
    }

    // Cancel and destroy this sentinel.
    fn cancel(mut self) {
        self.active = false;
    }
}

impl<'a> Drop for Sentinel<'a> {
    fn drop(&mut self) {
        if self.active {
            *self.thread_counter.lock().unwrap() -= 1;
            spawn_in_pool(self.jobs.clone(), self.thread_counter.clone(), self.thread_count_max.clone(), thread::current().name().unwrap().parse::<u8>().unwrap())
        }
    }
}

/// A thread pool used to execute functions in parallel.
///
/// Spawns `n` worker threads and replenishes the pool if any worker threads
/// panic.
///
/// # Example
///
/// ```
/// use threadpool::ThreadPool;
/// use std::sync::mpsc::channel;
///
/// let pool = ThreadPool::new(4);
///
/// let (tx, rx) = channel();
/// for i in 0..8 {
///     let tx = tx.clone();
///     pool.execute(move|| {
///         tx.send(i).unwrap();
///     });
/// }
///
/// assert_eq!(rx.iter().take(8).fold(0, |a, b| a + b), 28);
/// ```
#[derive(Clone)]
pub struct ThreadPool {
    // How the threadpool communicates with subthreads.
    //
    // This is the only such Sender, so when it is dropped all subthreads will
    // quit.
    jobs: Sender<Thunk<'static>>,
    job_receiver: Arc<Mutex<Receiver<Thunk<'static>>>>,
    active_count: Arc<Mutex<usize>>,
    max_count: Arc<Mutex<usize>>
}

impl ThreadPool {
    /// Spawns a new thread pool with `threads` threads.
    ///
    /// # Panics
    ///
    /// This function will panic if `threads` is 0.
    pub fn new(threads: usize) -> ThreadPool {
        assert!(threads >= 1);

        let (tx, rx) = channel::<Thunk<'static>>();
        let rx = Arc::new(Mutex::new(rx));
        let active_count = Arc::new(Mutex::new(0));
        let max_count = Arc::new(Mutex::new(threads));

        // Threadpool threads
        for id in 0..threads {
            spawn_in_pool(rx.clone(), active_count.clone(), max_count.clone(), id.clone() as u8);
        }

        ThreadPool {
            jobs: tx,
            job_receiver: rx.clone(),
            active_count: active_count,
            max_count: max_count
        }
    }

    /// Executes the function `job` on a thread in the pool.
    pub fn execute<F>(&self, job: F)
        where F : FnOnce() + Send + 'static
    {
        self.jobs.send(Box::new(move || job())).unwrap();
    }

    /// Returns the number of currently active threads.
    pub fn active_count(&self) -> usize {
        *self.active_count.lock().unwrap()
    }

    /// Returns the number of created threads
    pub fn max_count(&self) -> usize {
        *self.max_count.lock().unwrap()
    }

    /// Sets the number of threads to use as `threads`.
    /// Can be used to change the threadpool size during runtime
    pub fn set_threads(&mut self, threads: usize) {
        assert!(threads >= 1);
        let current_max = self.max_count.lock().unwrap().clone();
        *self.max_count.lock().unwrap() = threads;
        if threads > current_max {
            // Spawn new threads
            for id in 0..(threads - current_max) {
                spawn_in_pool(self.job_receiver.clone(), self.active_count.clone(), self.max_count.clone(), (current_max + id).clone() as u8);
            }
        }
    }
}

fn spawn_in_pool(jobs: Arc<Mutex<Receiver<Thunk<'static>>>>,
                 thread_counter: Arc<Mutex<usize>>,
                 thread_count_max: Arc<Mutex<usize>>,
                 id: u8) {
        let t = thread::Builder::new().name(id.to_string()).spawn(move || {

        println!("{:?}", thread::current().name().unwrap());
        // Will spawn a new thread on panic unless it is cancelled.
        let mut sentinel = Sentinel::new(&jobs, &thread_counter, &thread_count_max);

        loop {
            // clone values so that the mutexes are not held
            let thread_counter_val = thread_counter.lock().unwrap().clone();
            let thread_count_max_val = thread_count_max.lock().unwrap().clone();
            if thread_counter_val < thread_count_max_val {
                let message = {
                    // Only lock jobs for the time it takes
                    // to get a job, not run it.
                    let lock = jobs.lock().unwrap();
                    lock.recv()
                };

                match message {
                    Ok(job) => {
                        *thread_counter.lock().unwrap() += 1;
                        job.call_box();
                        sentinel.request_counter += 1;
                        println!("request count for thread {}: {}", id, sentinel.request_counter);
                        *thread_counter.lock().unwrap() -= 1;
                    },

                    // The Threadpool was dropped.
                    Err(..) => break
                }
            } else {
                break;
            }
        }

        sentinel.cancel();
    });
}

#[cfg(test)]
mod test {
    #![allow(deprecated)]
    use super::ThreadPool;
    use std::sync::mpsc::channel;
    use std::sync::{Arc, Barrier};
    use std::thread::sleep_ms;

    const TEST_TASKS: usize = 4;

    #[test]
    fn test_set_threads_increasing() {
        let new_thread_amount = 6;
        let mut pool = ThreadPool::new(TEST_TASKS);
        for _ in 0..TEST_TASKS {
            pool.execute(move || {
                loop {
                    sleep_ms(10000)
                }
            });
        }
        pool.set_threads(new_thread_amount);
        for _ in 0..(new_thread_amount - TEST_TASKS) {
            pool.execute(move || {
                loop {
                    sleep_ms(10000)
                }
            });
        }
        sleep_ms(1000);
        assert_eq!(pool.active_count(), new_thread_amount);
    }

    #[test]
    fn test_set_threads_decreasing() {
        let new_thread_amount = 2;
        let mut pool = ThreadPool::new(TEST_TASKS);
        for _ in 0..TEST_TASKS {
            pool.execute(move || {
                1 + 1;
            });
        }
        pool.set_threads(new_thread_amount);
        for _ in 0..new_thread_amount {
            pool.execute(move || {
                loop {
                    sleep_ms(10000)
                }
            });
        }
        sleep_ms(1000);
        assert_eq!(pool.active_count(), new_thread_amount);
    }

    #[test]
    fn test_active_count() {
        let pool = ThreadPool::new(TEST_TASKS);
        for _ in 0..TEST_TASKS {
            pool.execute(move|| {
                loop {
                    sleep_ms(10000);
                }
            });
        }
        sleep_ms(1000);
        let active_count = pool.active_count();
        assert_eq!(active_count, TEST_TASKS);
        let initialized_count = pool.max_count();
        assert_eq!(initialized_count, TEST_TASKS);
    }

    #[test]
    fn test_works() {
        let pool = ThreadPool::new(TEST_TASKS);

        let (tx, rx) = channel();
        for _ in 0..TEST_TASKS {
            let tx = tx.clone();
            pool.execute(move|| {
                tx.send(1).unwrap();
            });
        }

        assert_eq!(rx.iter().take(TEST_TASKS).fold(0, |a, b| a + b), TEST_TASKS);
    }

    #[test]
    #[should_panic]
    fn test_zero_tasks_panic() {
        ThreadPool::new(0);
    }

    #[test]
    fn test_recovery_from_subtask_panic() {
        let pool = ThreadPool::new(TEST_TASKS);

        // Panic all the existing threads.
        for _ in 0..TEST_TASKS {
            pool.execute(move|| -> () { panic!() });
        }

        // Ensure new threads were spawned to compensate.
        let (tx, rx) = channel();
        for _ in 0..TEST_TASKS {
            let tx = tx.clone();
            pool.execute(move|| {
                tx.send(1).unwrap();
            });
        }

        assert_eq!(rx.iter().take(TEST_TASKS).fold(0, |a, b| a + b), TEST_TASKS);
    }

    #[test]
    fn test_should_not_panic_on_drop_if_subtasks_panic_after_drop() {

        let pool = ThreadPool::new(TEST_TASKS);
        let waiter = Arc::new(Barrier::new(TEST_TASKS + 1));

        // Panic all the existing threads in a bit.
        for _ in 0..TEST_TASKS {
            let waiter = waiter.clone();
            pool.execute(move|| {
                waiter.wait();
                panic!();
            });
        }

        drop(pool);

        // Kick off the failure.
        waiter.wait();
    }
}
