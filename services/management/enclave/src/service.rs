// Licensed to the Apache Software Foundation (ASF) under one
// or more contributor license agreements.  See the NOTICE file
// distributed with this work for additional information
// regarding copyright ownership.  The ASF licenses this file
// to you under the Apache License, Version 2.0 (the
// "License"); you may not use this file except in compliance
// with the License.  You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the License is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.  See the License for the
// specific language governing permissions and limitations
// under the License.

use anyhow::{anyhow, Result};
use std::collections::HashMap;
use std::prelude::v1::*;
use std::sync::{Arc, SgxMutex as Mutex};
use teaclave_proto::teaclave_frontend_service::{
    ApproveTaskRequest, ApproveTaskResponse, AssignDataRequest, AssignDataResponse,
    CreateTaskRequest, CreateTaskResponse, GetFunctionRequest, GetFunctionResponse,
    GetInputFileRequest, GetInputFileResponse, GetOutputFileRequest, GetOutputFileResponse,
    GetTaskRequest, GetTaskResponse, InvokeTaskRequest, InvokeTaskResponse,
    RegisterFunctionRequest, RegisterFunctionResponse, RegisterFusionOutputRequest,
    RegisterFusionOutputResponse, RegisterInputFileRequest, RegisterInputFileResponse,
    RegisterInputFromOutputRequest, RegisterInputFromOutputResponse, RegisterOutputFileRequest,
    RegisterOutputFileResponse,
};
use teaclave_proto::teaclave_management_service::TeaclaveManagement;
use teaclave_proto::teaclave_storage_service::{
    EnqueueRequest, GetRequest, PutRequest, TeaclaveStorageClient,
};
use teaclave_rpc::endpoint::Endpoint;
use teaclave_rpc::Request;
use teaclave_service_enclave_utils::{ensure, teaclave_service};
use teaclave_types::Function;
#[cfg(test_mode)]
use teaclave_types::*;
use thiserror::Error;
use uuid::Uuid;

#[derive(Error, Debug)]
enum ServiceError {
    #[error("invalid request")]
    InvalidRequest,
    #[error("data error")]
    DataError,
    #[error("storage error")]
    StorageError,
    #[error("permission denied")]
    PermissionDenied,
    #[error("bad task")]
    BadTask,
}

impl From<ServiceError> for TeaclaveServiceResponseError {
    fn from(error: ServiceError) -> Self {
        TeaclaveServiceResponseError::RequestError(error.to_string())
    }
}

#[teaclave_service(teaclave_management_service, TeaclaveManagement, ServiceError)]
#[derive(Clone)]
pub(crate) struct TeaclaveManagementService {
    storage_client: Arc<Mutex<TeaclaveStorageClient>>,
}

impl TeaclaveManagement for TeaclaveManagementService {
    // access control: none
    fn register_input_file(
        &self,
        request: Request<RegisterInputFileRequest>,
    ) -> TeaclaveServiceResponseResult<RegisterInputFileResponse> {
        let user_id = self.get_request_user_id(request.metadata())?;
        let request = request.message;
        let input_file = TeaclaveInputFile::new(
            request.url,
            request.hash,
            request.crypto_info,
            vec![user_id],
        );

        self.write_to_db(&input_file)
            .map_err(|_| ServiceError::StorageError)?;

        let response = RegisterInputFileResponse {
            data_id: input_file.external_id(),
        };

        Ok(response)
    }

    // access control: none
    fn register_output_file(
        &self,
        request: Request<RegisterOutputFileRequest>,
    ) -> TeaclaveServiceResponseResult<RegisterOutputFileResponse> {
        let user_id = self.get_request_user_id(request.metadata())?;
        let request = request.message;
        let output_file = TeaclaveOutputFile::new(request.url, request.crypto_info, vec![user_id]);

        self.write_to_db(&output_file)
            .map_err(|_| ServiceError::StorageError)?;

        let response = RegisterOutputFileResponse {
            data_id: output_file.external_id(),
        };
        Ok(response)
    }

    // access control: user_id in owner_list
    fn register_fusion_output(
        &self,
        request: Request<RegisterFusionOutputRequest>,
    ) -> TeaclaveServiceResponseResult<RegisterFusionOutputResponse> {
        let user_id = self.get_request_user_id(request.metadata())?;

        let owner_list = request.message.owner_list;
        ensure!(
            owner_list.len() > 1 && owner_list.contains(&user_id),
            ServiceError::PermissionDenied
        );

        let output_file =
            TeaclaveOutputFile::new_fusion_data(owner_list).map_err(|_| ServiceError::DataError)?;

        self.write_to_db(&output_file)
            .map_err(|_| ServiceError::StorageError)?;

        let response = RegisterFusionOutputResponse {
            data_id: output_file.external_id(),
        };
        Ok(response)
    }

    // access control:
    // 1) user_id in output.owner
    // 2) hash != none
    fn register_input_from_output(
        &self,
        request: Request<RegisterInputFromOutputRequest>,
    ) -> TeaclaveServiceResponseResult<RegisterInputFromOutputResponse> {
        let user_id = self.get_request_user_id(request.metadata())?;

        let output: TeaclaveOutputFile = self
            .read_from_db(&request.message.data_id)
            .map_err(|_| ServiceError::PermissionDenied)?;

        ensure!(
            output.owner.contains(&user_id),
            ServiceError::PermissionDenied
        );

        let input =
            TeaclaveInputFile::from_output(output).map_err(|_| ServiceError::PermissionDenied)?;

        self.write_to_db(&input)
            .map_err(|_| ServiceError::StorageError)?;

        let response = RegisterInputFromOutputResponse {
            data_id: input.external_id(),
        };
        Ok(response)
    }

    // access control: output_file.owner contains user_id
    fn get_output_file(
        &self,
        request: Request<GetOutputFileRequest>,
    ) -> TeaclaveServiceResponseResult<GetOutputFileResponse> {
        let user_id = self.get_request_user_id(request.metadata())?;

        let output_file: TeaclaveOutputFile = self
            .read_from_db(&request.message.data_id)
            .map_err(|_| ServiceError::PermissionDenied)?;

        ensure!(
            output_file.owner.contains(&user_id),
            ServiceError::PermissionDenied
        );

        let response = GetOutputFileResponse {
            owner: output_file.owner,
            hash: output_file.hash.unwrap_or_else(|| "".to_string()),
        };
        Ok(response)
    }

    // access control: input_file.owner contains user_id
    fn get_input_file(
        &self,
        request: Request<GetInputFileRequest>,
    ) -> TeaclaveServiceResponseResult<GetInputFileResponse> {
        let user_id = self.get_request_user_id(request.metadata())?;

        let input_file: TeaclaveInputFile = self
            .read_from_db(&request.message.data_id)
            .map_err(|_| ServiceError::PermissionDenied)?;

        ensure!(
            input_file.owner.contains(&user_id),
            ServiceError::PermissionDenied
        );

        let response = GetInputFileResponse {
            owner: input_file.owner,
            hash: input_file.hash,
        };
        Ok(response)
    }

    // access_control: none
    fn register_function(
        &self,
        request: Request<RegisterFunctionRequest>,
    ) -> TeaclaveServiceResponseResult<RegisterFunctionResponse> {
        let user_id = self.get_request_user_id(request.metadata())?;

        let function = Function::from(request.message)
            .id(Uuid::new_v4())
            .owner(user_id);

        self.write_to_db(&function)
            .map_err(|_| ServiceError::StorageError)?;

        let response = RegisterFunctionResponse {
            function_id: function.external_id(),
        };
        Ok(response)
    }

    // access control: function.public || function.owner == user_id
    fn get_function(
        &self,
        request: Request<GetFunctionRequest>,
    ) -> TeaclaveServiceResponseResult<GetFunctionResponse> {
        let user_id = self.get_request_user_id(request.metadata())?;

        let function: Function = self
            .read_from_db(&request.message.function_id)
            .map_err(|_| ServiceError::PermissionDenied)?;

        ensure!(
            (function.public || function.owner == user_id),
            ServiceError::PermissionDenied
        );

        let response = GetFunctionResponse {
            name: function.name,
            description: function.description,
            owner: function.owner,
            executor_type: function.executor_type,
            payload: function.payload,
            public: function.public,
            arguments: function.arguments,
            inputs: function.inputs,
            outputs: function.outputs,
        };
        Ok(response)
    }

    // access control: none
    // when a task is created, following rules will be verified:
    // 1) arugments match function definition
    // 2) input match function definition
    // 3) output match function definition
    fn create_task(
        &self,
        request: Request<CreateTaskRequest>,
    ) -> TeaclaveServiceResponseResult<CreateTaskResponse> {
        let user_id = self.get_request_user_id(request.metadata())?;

        let request = request.message;

        let function: Function = self
            .read_from_db(&request.function_id)
            .map_err(|_| ServiceError::PermissionDenied)?;

        let mut task = Task::new(
            user_id,
            request.executor,
            &function,
            request.function_arguments,
            request.input_owners_map,
            request.output_owners_map,
        );

        task.check_function_compatibility(&function)
            .map_err(|_| ServiceError::BadTask)?;

        task.update_status(TaskStatus::Created);

        log::info!("CreateTask: {:?}", task);

        self.write_to_db(&task)
            .map_err(|_| ServiceError::StorageError)?;

        Ok(CreateTaskResponse {
            task_id: task.external_id(),
        })
    }

    // access control: task.participants.contains(&user_id)
    fn get_task(
        &self,
        request: Request<GetTaskRequest>,
    ) -> TeaclaveServiceResponseResult<GetTaskResponse> {
        let user_id = self.get_request_user_id(request.metadata())?;

        let task: Task = self
            .read_from_db(&request.message.task_id)
            .map_err(|_| ServiceError::PermissionDenied)?;

        ensure!(
            task.participants.contains(&user_id),
            ServiceError::PermissionDenied
        );

        log::info!("GetTask: {:?}", task);

        let response = GetTaskResponse {
            task_id: task.external_id(),
            creator: task.creator,
            function_id: task.function_id,
            function_owner: task.function_owner,
            function_arguments: task.function_arguments,
            input_owners_map: task.input_owners_map,
            output_owners_map: task.output_owners_map,
            participants: task.participants,
            approved_users: task.approved_users,
            input_map: task.input_map,
            output_map: task.output_map,
            result: task.result,
            status: task.status,
        };
        Ok(response)
    }

    // access control:
    // 1) task.participants.contains(user_id)
    // 2) task.status == Created
    // 3) user can use the data:
    //    * input file: user_id == input_file.owner contains user_id
    //    * output file: output_file.owner contains user_id && output_file.hash.is_none()
    // 4) the data can be assgined to the task:
    //    * input_owners_map or output_owners_map contains the data name
    //    * input file: OwnerList match input_file.owner
    //    * output file: OwnerList match output_file.owner
    fn assign_data(
        &self,
        request: Request<AssignDataRequest>,
    ) -> TeaclaveServiceResponseResult<AssignDataResponse> {
        let user_id = self.get_request_user_id(request.metadata())?;

        let request = request.message;

        let mut task: Task = self
            .read_from_db(&request.task_id)
            .map_err(|_| ServiceError::PermissionDenied)?;

        ensure!(
            task.participants.contains(&user_id),
            ServiceError::PermissionDenied
        );

        ensure!(
            task.status == TaskStatus::Created,
            ServiceError::PermissionDenied
        );

        for (data_name, data_id) in request.input_map.iter() {
            let file: TeaclaveInputFile = self
                .read_from_db(&data_id)
                .map_err(|_| ServiceError::PermissionDenied)?;
            task.assign_input(&user_id, data_name, &file)
                .map_err(|_| ServiceError::PermissionDenied)?;
        }

        for (data_name, data_id) in request.output_map.iter() {
            let file: TeaclaveOutputFile = self
                .read_from_db(&data_id)
                .map_err(|_| ServiceError::PermissionDenied)?;
            task.assign_output(&user_id, data_name, &file)
                .map_err(|_| ServiceError::PermissionDenied)?;
        }

        if task.all_data_assigned() {
            task.update_status(TaskStatus::DataAssigned);
        }

        log::info!("AssignData: {:?}", task);

        self.write_to_db(&task)
            .map_err(|_| ServiceError::StorageError)?;

        Ok(AssignDataResponse)
    }

    // access_control:
    // 1) task status == Ready
    // 2) user_id in task.participants
    fn approve_task(
        &self,
        request: Request<ApproveTaskRequest>,
    ) -> TeaclaveServiceResponseResult<ApproveTaskResponse> {
        let user_id = self.get_request_user_id(request.metadata())?;

        let request = request.message;
        let mut task: Task = self
            .read_from_db(&request.task_id)
            .map_err(|_| ServiceError::PermissionDenied)?;

        ensure!(
            task.participants.contains(&user_id),
            ServiceError::PermissionDenied
        );

        ensure!(
            task.status == TaskStatus::DataAssigned,
            ServiceError::PermissionDenied,
        );

        task.approved_users.insert(user_id);
        if task.all_approved() {
            task.update_status(TaskStatus::Approved);
        }

        log::info!("ApproveTask: approve:{:?}", task);

        self.write_to_db(&task)
            .map_err(|_| ServiceError::StorageError)?;

        Ok(ApproveTaskResponse)
    }

    // access_control:
    // 1) task status == Approved
    // 2) user_id == task.creator
    fn invoke_task(
        &self,
        request: Request<InvokeTaskRequest>,
    ) -> TeaclaveServiceResponseResult<InvokeTaskResponse> {
        let user_id = self.get_request_user_id(request.metadata())?;
        let request = request.message;

        let mut task: Task = self
            .read_from_db(&request.task_id)
            .map_err(|_| ServiceError::PermissionDenied)?;

        log::info!("InvokeTask: get task: {:?}", task);

        ensure!(task.creator == user_id, ServiceError::PermissionDenied);
        ensure!(
            task.status == TaskStatus::Approved,
            ServiceError::PermissionDenied
        );

        let function: Function = self
            .read_from_db(&task.function_id)
            .map_err(|_| ServiceError::PermissionDenied)?;

        log::info!("InvokeTask: get function: {:?}", function);

        let mut input_map: HashMap<String, FunctionInputFile> = HashMap::new();
        let mut output_map: HashMap<String, FunctionOutputFile> = HashMap::new();
        for (data_name, data_id) in task.input_map.iter() {
            let input_file: TeaclaveInputFile = self
                .read_from_db(&data_id)
                .map_err(|_| ServiceError::PermissionDenied)?;
            let input_data = FunctionInputFile::from_teaclave_input_file(&input_file);
            input_map.insert(data_name.to_string(), input_data);
        }

        for (data_name, data_id) in task.output_map.iter() {
            let output_file: TeaclaveOutputFile = self
                .read_from_db(&data_id)
                .map_err(|_| ServiceError::PermissionDenied)?;
            ensure!(output_file.hash.is_none(), ServiceError::PermissionDenied);
            let output_data = FunctionOutputFile::from_teaclave_output_file(&output_file);
            output_map.insert(data_name.to_string(), output_data);
        }

        let function_arguments = task.function_arguments.clone();

        let staged_task = StagedTask::new()
            .task_id(task.task_id)
            .executor(task.executor)
            .executor_type(function.executor_type)
            .function_id(function.id)
            .function_payload(function.payload)
            .function_arguments(function_arguments)
            .input_data(input_map)
            .output_data(output_map);

        self.enqueue_to_db(StagedTask::get_queue_key().as_bytes(), &staged_task)?;
        task.status = TaskStatus::Running;

        log::info!("InvokeTask: staged task: {:?}", task);

        self.write_to_db(&task)
            .map_err(|_| ServiceError::StorageError)?;
        Ok(InvokeTaskResponse)
    }
}

impl TeaclaveManagementService {
    pub(crate) fn new(storage_service_endpoint: Endpoint) -> Result<Self> {
        let mut i = 0;
        let channel = loop {
            match storage_service_endpoint.connect() {
                Ok(channel) => break channel,
                Err(_) => {
                    anyhow::ensure!(i < 10, "failed to connect to storage service");
                    log::debug!("Failed to connect to storage service, retry {}", i);
                    i += 1;
                }
            }
            std::thread::sleep(std::time::Duration::from_secs(3));
        };
        let storage_client = Arc::new(Mutex::new(TeaclaveStorageClient::new(channel)?));
        let service = Self { storage_client };

        #[cfg(test_mode)]
        service.add_mock_data()?;

        Ok(service)
    }

    fn get_request_user_id(
        &self,
        meta: &HashMap<String, String>,
    ) -> TeaclaveServiceResponseResult<UserID> {
        let user_id = meta.get("id").ok_or_else(|| ServiceError::InvalidRequest)?;
        Ok(user_id.to_string().into())
    }

    fn write_to_db(&self, item: &impl Storable) -> Result<()> {
        let k = item.key();
        let v = item.to_vec()?;
        let put_request = PutRequest::new(k.as_slice(), v.as_slice());
        let _put_response = self
            .storage_client
            .clone()
            .lock()
            .map_err(|_| anyhow!("Cannot lock storage client"))?
            .put(put_request)?;
        Ok(())
    }

    fn read_from_db<T: Storable>(&self, key: &ExternalID) -> Result<T> {
        anyhow::ensure!(T::match_prefix(&key.prefix), "Key prefix doesn't match.");

        let request = GetRequest::new(key.to_bytes());
        let response = self
            .storage_client
            .clone()
            .lock()
            .map_err(|_| anyhow!("Cannot lock storage client"))?
            .get(request)?;
        T::from_slice(response.value.as_slice())
    }

    fn enqueue_to_db(&self, key: &[u8], item: &impl Storable) -> TeaclaveServiceResponseResult<()> {
        let value = item.to_vec().map_err(|_| ServiceError::DataError)?;
        let enqueue_request = EnqueueRequest::new(key, value);
        let _enqueue_response = self
            .storage_client
            .clone()
            .lock()
            .map_err(|_| ServiceError::StorageError)?
            .enqueue(enqueue_request)?;
        Ok(())
    }

    #[cfg(test_mode)]
    fn add_mock_data(&self) -> Result<()> {
        let mut output_file =
            TeaclaveOutputFile::new_fusion_data(vec!["mock_user1", "frontend_user"])?;
        output_file.uuid = Uuid::parse_str("00000000-0000-0000-0000-000000000001")?;
        output_file.hash = Some("deadbeef".to_string());
        self.write_to_db(&output_file)?;

        let mut output_file =
            TeaclaveOutputFile::new_fusion_data(vec!["mock_user2", "mock_user3"])?;
        output_file.uuid = Uuid::parse_str("00000000-0000-0000-0000-000000000002")?;
        output_file.hash = Some("deadbeef".to_string());
        self.write_to_db(&output_file)?;

        let mut input_file = TeaclaveInputFile::from_output(output_file)?;
        input_file.uuid = Uuid::parse_str("00000000-0000-0000-0000-000000000002")?;
        self.write_to_db(&input_file)?;

        let function_input = FunctionInput::new("input", "input_desc");
        let function_output = FunctionOutput::new("output", "output_desc");
        let function_input2 = FunctionInput::new("input2", "input_desc");
        let function_output2 = FunctionOutput::new("output2", "output_desc");

        let function = Function::new()
            .id(Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap())
            .name("mock-func-1")
            .description("mock-desc")
            .payload(b"mock-payload".to_vec())
            .public(true)
            .arguments(vec!["arg1".to_string(), "arg2".to_string()])
            .inputs(vec![function_input, function_input2])
            .outputs(vec![function_output, function_output2])
            .owner("teaclave".to_string());

        self.write_to_db(&function)?;

        let function_output = FunctionOutput::new("output", "output_desc");
        let function = Function::new()
            .id(Uuid::parse_str("00000000-0000-0000-0000-000000000002").unwrap())
            .name("mock-func-2")
            .description("mock-desc")
            .payload(b"mock-payload".to_vec())
            .public(true)
            .arguments(vec!["arg1".to_string()])
            .outputs(vec![function_output])
            .owner("teaclave".to_string());

        self.write_to_db(&function)?;
        Ok(())
    }
}

#[cfg(feature = "enclave_unit_test")]
pub mod tests {
    use super::*;
    use std::collections::HashMap;
    use teaclave_types::{hashmap, FileCrypto, FunctionArguments, FunctionInput, FunctionOutput};
    use url::Url;

    pub fn handle_input_file() {
        let url = Url::parse("s3://bucket_id/path?token=mock_token").unwrap();
        let hash = "a6d604b5987b693a19d94704532b5d928c2729f24dfd40745f8d03ac9ac75a8b".to_string();
        let input_file =
            TeaclaveInputFile::new(url, hash, FileCrypto::default(), vec!["mock_user"]);
        assert!(TeaclaveInputFile::match_prefix(&input_file.key_string()));
        let value = input_file.to_vec().unwrap();
        let deserialized_file = TeaclaveInputFile::from_slice(&value).unwrap();
        info!("file: {:?}", deserialized_file);
    }

    pub fn handle_output_file() {
        let url = Url::parse("s3://bucket_id/path?token=mock_token").unwrap();
        let output_file = TeaclaveOutputFile::new(url, FileCrypto::default(), vec!["mock_user"]);
        assert!(TeaclaveOutputFile::match_prefix(&output_file.key_string()));
        let value = output_file.to_vec().unwrap();
        let deserialized_file = TeaclaveOutputFile::from_slice(&value).unwrap();
        info!("file: {:?}", deserialized_file);
    }

    pub fn handle_function() {
        let function_input = FunctionInput::new("input", "input_desc");
        let function_output = FunctionOutput::new("output", "output_desc");
        let function = Function::new()
            .id(Uuid::new_v4())
            .name("mock_function")
            .description("mock function")
            .payload(b"python script".to_vec())
            .arguments(vec!["arg".to_string()])
            .inputs(vec![function_input])
            .outputs(vec![function_output])
            .public(true)
            .owner("mock_user");
        assert!(Function::match_prefix(&function.key_string()));
        let value = function.to_vec().unwrap();
        let deserialized_function = Function::from_slice(&value).unwrap();
        info!("function: {:?}", deserialized_function);
    }

    pub fn handle_task() {
        let function = Function::new()
            .id(Uuid::new_v4())
            .name("mock_function")
            .description("mock function")
            .payload(b"python script".to_vec())
            .arguments(vec!["arg".to_string()])
            .public(true)
            .owner("mock_user");
        let function_arguments = FunctionArguments::new(hashmap!("arg" => "data"));

        let task = Task::new(
            UserID::from("mock_user"),
            Executor::MesaPy,
            &function,
            function_arguments,
            HashMap::new(),
            HashMap::new(),
        );

        assert!(Task::match_prefix(&task.key_string()));
        let value = task.to_vec().unwrap();
        let deserialized_task = Task::from_slice(&value).unwrap();
        info!("task: {:?}", deserialized_task);
    }

    pub fn handle_staged_task() {
        let function = Function::new()
            .id(Uuid::new_v4())
            .name("mock_function")
            .description("mock function")
            .payload(b"python script".to_vec())
            .public(true)
            .owner("mock_user");

        let url = Url::parse("s3://bucket_id/path?token=mock_token").unwrap();
        let hash = "a6d604b5987b693a19d94704532b5d928c2729f24dfd40745f8d03ac9ac75a8b".to_string();
        let input_data = FunctionInputFile::new(url.clone(), hash, FileCrypto::default());
        let output_data = FunctionOutputFile::new(url, FileCrypto::default());

        let staged_task = StagedTask::new()
            .task_id(Uuid::new_v4())
            .executor(Executor::MesaPy)
            .function_payload(function.payload)
            .function_arguments(hashmap!("arg" => "data"))
            .input_data(hashmap!("input" => input_data))
            .output_data(hashmap!("output" => output_data));

        let value = staged_task.to_vec().unwrap();
        let deserialized_data = StagedTask::from_slice(&value).unwrap();
        info!("staged task: {:?}", deserialized_data);
    }
}
