using Newtonsoft.Json;

namespace Models
{
    public struct JoinRoomResult
    {
        public string invite;
        public string addr;
    }
    public struct LoginResult
    {
        public string token;
    }
    public struct Reqid
    {
        [JsonProperty("req_id")]
        public string id;
    }
}