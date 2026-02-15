use alloc::collections::VecDeque;
use alloc::sync::Arc;
use alloc::task::Wake;
use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll, Waker, RawWaker, RawWakerVTable};
use alloc::boxed::Box;

pub struct Task {
    future: Pin<Box<dyn Future<Output = ()> + Send + 'static>>,
}

impl Task {
    pub fn new(future: impl Future<Output = ()> + Send + 'static) -> Task {
        Task {
            future: Box::pin(future),
        }
    }

    fn poll(&mut self, context: &mut Context) -> Poll<()> {
        self.future.as_mut().poll(context)
    }
}

pub struct Executor {
    task_queue: VecDeque<Task>,
}

impl Executor {
    pub fn new() -> Executor {
        Executor {
            task_queue: VecDeque::new(),
        }
    }

    pub fn spawn(&mut self, task: Task) {
        self.task_queue.push_back(task)
    }

    pub fn run_ready_tasks(&mut self) {
        let mut tasks_to_run = self.task_queue.len();
        let waker = dummy_waker();
        let mut context = Context::from_waker(&waker);
        
        while tasks_to_run > 0 {
            if let Some(mut task) = self.task_queue.pop_front() {
                match task.poll(&mut context) {
                    Poll::Ready(()) => {
                        // task done
                    }
                    Poll::Pending => {
                        self.task_queue.push_back(task);
                    }
                }
            }
            tasks_to_run -= 1;
        }
    }
    
    // Run one check pass
    pub fn poll(&mut self) {
        self.run_ready_tasks();
    }
}

fn dummy_waker() -> Waker {
    static VTABLE: RawWakerVTable = RawWakerVTable::new(
        |_| RawWaker::new(core::ptr::null(), &VTABLE),
        |_| {},
        |_| {},
        |_| {},
    );
    unsafe { Waker::from_raw(RawWaker::new(core::ptr::null(), &VTABLE)) }
}
